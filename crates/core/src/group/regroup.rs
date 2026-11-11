//! Regrouping (spec §5.3, §5.4, §5.4.1): the one pure function that turns the
//! cached per-file data and the thresholds into a job's automatic groups.
//! The analysis calls it once, with Stage 3 off; the engine calls it again
//! whenever a slider moves, an embedding arrives or the user edits a group.
//!
//! In order:
//!
//! 1. Stage 1 buckets every hashed file, so the timeline and the bucket keys
//!    never depend on the user's edits.
//! 2. The pool of each bucket is its files minus the members of the manual
//!    groups and the photos the user pinned to the tray. Stage 2 links each
//!    pool by the Hamming threshold and by its bracket runs.
//! 3. Stage 3 checks each linked component whose maximum hash distance is
//!    above the floor: its members whose cosine similarity reaches the
//!    threshold link, and so do the ends of the component's bracket-run
//!    links, so a bracket the camera data confirms is never split. Each
//!    component of that graph becomes a group; a photo left alone goes to
//!    the tray.
//! 4. Each group gets its layer order.
//! 5. Each group is matched with the existing automatic groups by exact
//!    member set, so an unchanged group keeps its id and its review.
//!
//! Nothing here reads or writes anything: the caller hands in the data and
//! stores the result.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};

use super::link::UnionFind;
use super::{FileFacts, bracket_run_links, bucket, link, max_pairwise_distance, order_layers};
use crate::hash::HashPair;
use crate::model::{GroupStatus, MlStatus};

/// The parameters of one regrouping, from the job's config.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Thresholds {
    /// Stage 2: the largest normalised hash distance at which two photos link.
    pub hamming: u32,
    /// Stage 3 checks a group whose maximum hash distance is above this;
    /// `u32::MAX` turns Stage 3 off.
    pub floor: u32,
    /// Stage 3: the smallest cosine similarity at which two photos link.
    pub cosine: f32,
    /// Stage 1: the largest gap between consecutive shots of one bucket.
    pub time_window_seconds: u32,
}

/// An automatic group from the previous regrouping, with its review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExistingGroup {
    pub id: i64,
    /// File ids, in any order.
    pub members: Vec<usize>,
    pub status: GroupStatus,
    /// Its `look again` flag, which a regrouping that reuses it keeps: only
    /// the user's decision clears it (spec §5.4.1).
    pub review_reset: bool,
}

/// A group the user made or edited. Regrouping leaves it out of the pool and
/// never returns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManualGroup {
    pub id: i64,
    /// File ids in layer order (`[0]` is the top layer), as `order_layers`
    /// gives it: the store keeps them in this order. Regrouping only needs
    /// the set.
    pub members: Vec<usize>,
}

/// What [`regroup`] works from. File ids are indices into `facts` and
/// `hashes`.
#[derive(Clone, Copy)]
pub struct RegroupInput<'a> {
    pub facts: &'a [FileFacts],
    /// The normalised hashes; `None` for a file that is not grouped.
    pub hashes: &'a [Option<HashPair>],
    pub thresholds: Thresholds,
    /// A file's embedding for the active model, or `None` when it has none
    /// yet. `None` here means ML is unavailable. The embeddings of one model
    /// all have the same length.
    pub ml: Option<&'a dyn Fn(usize) -> Option<&'a [f32]>>,
    pub manual: &'a [ManualGroup],
    /// The photos the user put in the tray: they stay there.
    pub pinned: &'a [usize],
    /// The automatic groups the store holds now.
    pub existing: &'a [ExistingGroup],
}

/// One automatic group of a regrouping.
#[derive(Debug, Clone, PartialEq)]
pub struct RegroupedGroup {
    /// `Some` when an existing group with the same members is reused.
    pub id: Option<i64>,
    /// File ids in layer order: `[0]` is the top layer.
    pub members: Vec<usize>,
    /// The key of the Stage-1 bucket the group came from.
    pub bucket: String,
    /// The largest normalised hash distance between two members.
    pub max_distance: u32,
    /// The reused group's status, or `Proposed` for a new group.
    pub status: GroupStatus,
    /// A decision the user made on these photos no longer applies as it was:
    /// a new group with a member that was in a confirmed or rejected group,
    /// or in a flagged proposed one; or a reused group that was flagged.
    pub review_reset: bool,
    pub ml_status: MlStatus,
    /// The mean pairwise cosine similarity of the members, for a group
    /// Stage 3 confirmed or split off; `None` otherwise.
    pub ml_score: Option<f32>,
}

/// The result of [`regroup`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Regrouped {
    /// By Stage-1 bucket, in bucket order, then by smallest member id.
    pub groups: Vec<RegroupedGroup>,
    /// Pooled photos in no group, and the pinned photos: ascending. A pinned
    /// photo without a hash or in a manual group is left out.
    pub unstacked: Vec<usize>,
    /// The members of `Pending` groups that have no embedding: ascending.
    pub needs_embeddings: Vec<usize>,
    /// The existing groups no new group matched: the caller deletes them.
    /// Ascending.
    pub removed: Vec<i64>,
}

/// Regroups the hashed files of a job (see the module docs).
///
/// # Panics
///
/// If `facts` and `hashes` differ in length.
pub fn regroup(input: &RegroupInput) -> Regrouped {
    let RegroupInput {
        facts,
        hashes,
        thresholds,
        ..
    } = *input;
    assert_eq!(facts.len(), hashes.len(), "one hash slot per file");

    let manual: HashSet<usize> = input
        .manual
        .iter()
        .flat_map(|group| group.members.iter().copied())
        .collect();
    let hashed: Vec<usize> = (0..hashes.len())
        .filter(|&id| hashes[id].is_some())
        .collect();
    let mut tray: Vec<usize> = input
        .pinned
        .iter()
        .copied()
        .filter(|id| hashed.binary_search(id).is_ok() && !manual.contains(id))
        .collect();
    tray.sort_unstable();
    tray.dedup();
    let pooled = |id: &usize| !manual.contains(id) && tray.binary_search(id).is_err();

    // Stage 1 places every hashed file, edited or not, so that taking a photo
    // out of the pool never splits its bucket (a bucket chains through each
    // of its shots) or renumbers the bucket keys. Its warnings are the
    // analysis' to report, not a regrouping's.
    let (buckets, _) = bucket(facts, &hashed, thresholds.time_window_seconds);
    let mut reviews = Reviews::new(input.existing);
    let mut regrouped = Regrouped {
        unstacked: tray.clone(),
        ..Regrouped::default()
    };
    for bucket in buckets {
        // Only the pool links: the runs walk its members alone, so a run
        // steps over a photo the user took out.
        let pool: Vec<usize> = bucket.members.iter().copied().filter(pooled).collect();
        let runs = bracket_run_links(&pool, facts, hashes);
        let linked = link(&pool, hashes, thresholds.hamming, &runs);
        regrouped.unstacked.extend(linked.singletons);
        let mut parts = Vec::new();
        for component in linked.groups {
            let checked = check(component, &runs, input);
            parts.extend(checked.parts);
            regrouped.unstacked.extend(checked.singletons);
            regrouped.needs_embeddings.extend(checked.missing);
        }
        // A part is ascending, so this is the order of the smallest ids.
        parts.sort_by_key(|part| part.members[0]);
        for part in parts {
            let (id, status, review_reset) = reviews.review(&part.members);
            let max_distance = max_pairwise_distance(&part.members, hashes);
            let mut members = part.members;
            order_layers(&mut members, facts);
            regrouped.groups.push(RegroupedGroup {
                id,
                members,
                bucket: bucket.key.clone(),
                max_distance,
                status,
                review_reset,
                ml_status: part.ml_status,
                ml_score: part.ml_score,
            });
        }
    }
    regrouped.unstacked.sort_unstable();
    regrouped.needs_embeddings.sort_unstable();
    regrouped.removed = reviews.unmatched();
    regrouped
}

/// A group before its layer order and its review.
struct Part {
    /// Ascending.
    members: Vec<usize>,
    ml_status: MlStatus,
    ml_score: Option<f32>,
}

/// What Stage 3 made of one Stage-2 component.
struct Checked {
    parts: Vec<Part>,
    /// Members the embeddings split off on their own: they go to the tray.
    singletons: Vec<usize>,
    /// Members without an embedding, which make the component `Pending`.
    missing: Vec<usize>,
}

impl Checked {
    /// The component kept whole, with no score.
    fn whole(members: Vec<usize>, ml_status: MlStatus) -> Self {
        Self {
            parts: vec![Part {
                members,
                ml_status,
                ml_score: None,
            }],
            singletons: Vec::new(),
            missing: Vec::new(),
        }
    }
}

/// Stage 3 on one Stage-2 `component` (ascending ids). `runs` are the
/// bracket-run links of its bucket: they stay links whatever the embeddings
/// say.
///
/// - At or below the floor: kept whole, `None`.
/// - ML unavailable: kept whole, `Skipped`.
/// - A member without an embedding: kept whole, `Pending`, and the members
///   without one are `missing`.
/// - Otherwise the members whose cosine similarity reaches the threshold
///   link, and so do the ends of the run links. One component of that graph
///   is `Confirmed`; with several, each of two or more members is `Split` and
///   each single member goes to the tray. A part's score is its mean pairwise
///   cosine similarity.
fn check(component: Vec<usize>, runs: &[(usize, usize)], input: &RegroupInput) -> Checked {
    if max_pairwise_distance(&component, input.hashes) <= input.thresholds.floor {
        return Checked::whole(component, MlStatus::None);
    }
    let Some(embed) = input.ml else {
        return Checked::whole(component, MlStatus::Skipped);
    };
    let embeddings: Vec<Option<&[f32]>> = component.iter().map(|&id| embed(id)).collect();
    let missing: Vec<usize> = component
        .iter()
        .zip(&embeddings)
        .filter(|(_, embedding)| embedding.is_none())
        .map(|(&id, _)| id)
        .collect();
    if !missing.is_empty() {
        return Checked {
            missing,
            ..Checked::whole(component, MlStatus::Pending)
        };
    }
    let embeddings: Vec<&[f32]> = embeddings.into_iter().flatten().collect();
    let similarity = Similarity::new(&embeddings);

    let mut sets = UnionFind::new(component.len());
    for i in 0..component.len() {
        for j in i + 1..component.len() {
            if similarity.get(i, j) >= input.thresholds.cosine {
                sets.union(i, j);
            }
        }
    }
    let index_of = |id: usize| component.binary_search(&id).ok();
    for &(a, b) in runs {
        if let (Some(i), Some(j)) = (index_of(a), index_of(b)) {
            sets.union(i, j);
        }
    }

    let sets = sets.components();
    let ml_status = if sets.len() == 1 {
        MlStatus::Confirmed
    } else {
        MlStatus::Split
    };
    let mut checked = Checked {
        parts: Vec::new(),
        singletons: Vec::new(),
        missing: Vec::new(),
    };
    for set in sets {
        if let [i] = set[..] {
            checked.singletons.push(component[i]);
        } else {
            checked.parts.push(Part {
                ml_score: Some(similarity.mean(&set)),
                members: set.iter().map(|&i| component[i]).collect(),
                ml_status,
            });
        }
    }
    checked
}

/// The cosine similarities between the members of one component, by their
/// index in it.
struct Similarity {
    n: usize,
    values: Vec<f32>,
}

impl Similarity {
    fn new(embeddings: &[&[f32]]) -> Self {
        let n = embeddings.len();
        let mut values = vec![0.0; n * n];
        for i in 0..n {
            for j in i + 1..n {
                let value = cosine(embeddings[i], embeddings[j]);
                values[i * n + j] = value;
                values[j * n + i] = value;
            }
        }
        Self { n, values }
    }

    fn get(&self, i: usize, j: usize) -> f32 {
        self.values[i * self.n + j]
    }

    /// The mean similarity of the pairs of `indices` (at least two). The
    /// values are summed in ascending order, so the mean does not depend on
    /// how the files are numbered.
    fn mean(&self, indices: &[usize]) -> f32 {
        let mut pairs: Vec<f32> = indices
            .iter()
            .enumerate()
            .flat_map(|(k, &i)| indices[k + 1..].iter().map(move |&j| self.get(i, j)))
            .collect();
        pairs.sort_by(f32::total_cmp);
        let sum: f64 = pairs.iter().map(|&value| f64::from(value)).sum();
        (sum / pairs.len() as f64) as f32
    }
}

/// The cosine similarity of two embeddings of one model; 0 when either is
/// all zeros. It is symmetric to the bit, which [`Similarity::mean`] needs.
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len(), "embeddings of one model");
    let (mut dot, mut a_norm, mut b_norm) = (0.0_f64, 0.0_f64, 0.0_f64);
    for (&x, &y) in a.iter().zip(b) {
        let (x, y) = (f64::from(x), f64::from(y));
        dot += x * y;
        a_norm += x * x;
        b_norm += y * y;
    }
    let norms = (a_norm * b_norm).sqrt();
    if norms > 0.0 {
        (dot / norms) as f32
    } else {
        0.0
    }
}

/// The existing automatic groups, looked up by member set (spec §5.4.1).
struct Reviews<'a> {
    existing: &'a [ExistingGroup],
    /// Each member set (ascending, deduplicated) and its group. Two groups
    /// with one set (which the store never holds) keep the smaller id, so
    /// the choice does not depend on their order.
    by_members: HashMap<Vec<usize>, &'a ExistingGroup>,
    /// The files a decision was about that may no longer stand: those in a
    /// confirmed or rejected group, or in a proposed one flagged to look at
    /// again. Counting the flagged ones passes a flag on to the group that
    /// replaces a flagged one, so whether a stack says `look again` does not
    /// depend on how many regroupings a slider drag happened to run, as long
    /// as each leaves some of those photos in a group. The tray keeps no
    /// lineage: a stack whose members all fall to it loses the flag (spec
    /// §5.4.1).
    decided: HashSet<usize>,
    /// The ids of the groups a new group reused.
    reused: HashSet<i64>,
}

impl<'a> Reviews<'a> {
    fn new(existing: &'a [ExistingGroup]) -> Self {
        let mut by_members: HashMap<Vec<usize>, &ExistingGroup> = HashMap::new();
        for group in existing {
            let mut members = group.members.clone();
            members.sort_unstable();
            members.dedup();
            match by_members.entry(members) {
                Entry::Vacant(entry) => {
                    entry.insert(group);
                }
                Entry::Occupied(mut entry) => {
                    if group.id < entry.get().id {
                        entry.insert(group);
                    }
                }
            }
        }
        let decided = existing
            .iter()
            .filter(|group| {
                matches!(group.status, GroupStatus::Confirmed | GroupStatus::Rejected)
                    || (group.status == GroupStatus::Proposed && group.review_reset)
            })
            .flat_map(|group| group.members.iter().copied())
            .collect();
        Self {
            existing,
            by_members,
            decided,
            reused: HashSet::new(),
        }
    }

    /// The id, status and `review_reset` of the new group of `members`
    /// (ascending, distinct): those of the existing group with that member
    /// set, which is then used up (its flag kept until the user decides it);
    /// else a new `Proposed` group, reset when one of its members was in a
    /// confirmed or rejected group, or in a flagged proposed one.
    fn review(&mut self, members: &[usize]) -> (Option<i64>, GroupStatus, bool) {
        match self.by_members.remove(members) {
            Some(group) => {
                self.reused.insert(group.id);
                (Some(group.id), group.status, group.review_reset)
            }
            None => {
                let reset = members.iter().any(|id| self.decided.contains(id));
                (None, GroupStatus::Proposed, reset)
            }
        }
    }

    /// The ids of the existing groups no new group reused, ascending.
    fn unmatched(&self) -> Vec<i64> {
        let mut ids: Vec<i64> = self
            .existing
            .iter()
            .map(|group| group.id)
            .filter(|id| !self.reused.contains(id))
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use chrono::{NaiveDate, NaiveDateTime, TimeDelta};

    use super::*;
    use crate::exif::{BracketInfo, BracketMode, ExifData};

    const THRESHOLDS: Thresholds = Thresholds {
        hamming: 8,
        floor: 6,
        cosine: 0.85,
        time_window_seconds: 4,
    };

    fn at(seconds: i64) -> NaiveDateTime {
        let start = NaiveDate::from_ymd_opt(2026, 11, 7)
            .and_then(|d| d.and_hms_opt(10, 0, 0))
            .unwrap();
        start + TimeDelta::seconds(seconds)
    }

    /// A photo from camera `model`, taken `seconds` after a fixed instant.
    fn shot_on(model: &str, path: &str, seconds: i64) -> FileFacts {
        FileFacts {
            path: path.into(),
            exif: Some(ExifData {
                model: Some(model.into()),
                capture_time: Some(at(seconds)),
                ..ExifData::default()
            }),
            mtime_ms: 0,
        }
    }

    fn photo(path: &str, seconds: i64) -> FileFacts {
        shot_on("Body", path, seconds)
    }

    /// The `number`-th shot of an exposure bracket.
    fn bracket_shot(path: &str, seconds: i64, number: u16) -> FileFacts {
        let mut file = photo(path, seconds);
        file.exif.as_mut().unwrap().bracket = Some(BracketInfo {
            mode: BracketMode::Exposure,
            shot_number: Some(number),
            value_ev: None,
        });
        file
    }

    fn no_exif(path: &str) -> FileFacts {
        FileFacts {
            path: path.into(),
            exif: None,
            mtime_ms: 0,
        }
    }

    /// A hash `bits` away from `hash(0)`, so `hash(a)` and `hash(b)` are
    /// `|a - b|` apart.
    fn hash(bits: u32) -> Option<HashPair> {
        let ones = |n: u32| if n == 0 { 0 } else { u64::MAX >> (64 - n) };
        Some(HashPair {
            phash: ones(bits.min(64)),
            dhash: ones(bits.saturating_sub(64)),
        })
    }

    /// Photos `a/{i}.jpg`, one second apart from `start`, with these hashes.
    fn burst(start: i64, bits: &[u32]) -> (Vec<FileFacts>, Vec<Option<HashPair>>) {
        let facts = (0..bits.len())
            .map(|i| photo(&format!("a/{i}.jpg"), start + i as i64))
            .collect();
        (facts, bits.iter().map(|&b| hash(b)).collect())
    }

    /// Appends a second burst, ten seconds after the first one ends, so it
    /// gets its own bucket.
    fn then_burst(
        (mut facts, mut hashes): (Vec<FileFacts>, Vec<Option<HashPair>>),
        bits: &[u32],
    ) -> (Vec<FileFacts>, Vec<Option<HashPair>>) {
        let start = facts.len();
        for (i, &b) in bits.iter().enumerate() {
            let id = start + i;
            facts.push(photo(
                &format!("a/{id}.jpg"),
                10 * start as i64 + 100 + i as i64,
            ));
            hashes.push(hash(b));
        }
        (facts, hashes)
    }

    fn input<'a>(facts: &'a [FileFacts], hashes: &'a [Option<HashPair>]) -> RegroupInput<'a> {
        RegroupInput {
            facts,
            hashes,
            thresholds: THRESHOLDS,
            ml: None,
            manual: &[],
            pinned: &[],
            existing: &[],
        }
    }

    /// File `id`'s embedding in `table`. The tests wrap it in a closure in
    /// place, as the engine does: an `impl Fn` returned from a function could
    /// not be borrowed for the lifetime of the data it hands out.
    fn embedding(table: &[Option<Vec<f32>>], id: usize) -> Option<&[f32]> {
        table.get(id).and_then(|v| v.as_deref())
    }

    /// The unit vector `i` of `n` dimensions.
    fn unit(i: usize, n: usize) -> Option<Vec<f32>> {
        let mut v = vec![0.0; n];
        v[i] = 1.0;
        Some(v)
    }

    fn existing(id: i64, members: &[usize], status: GroupStatus) -> ExistingGroup {
        ExistingGroup {
            id,
            members: members.to_vec(),
            status,
            review_reset: false,
        }
    }

    fn members(regrouped: &Regrouped) -> Vec<Vec<usize>> {
        regrouped.groups.iter().map(|g| g.members.clone()).collect()
    }

    fn ml_statuses(regrouped: &Regrouped) -> Vec<MlStatus> {
        regrouped.groups.iter().map(|g| g.ml_status).collect()
    }

    /// A new group as the analysis proposes it: no review, no Stage 3.
    fn proposed(members: &[usize], bucket: &str, max_distance: u32) -> RegroupedGroup {
        RegroupedGroup {
            id: None,
            members: members.to_vec(),
            bucket: bucket.into(),
            max_distance,
            status: GroupStatus::Proposed,
            review_reset: false,
            ml_status: MlStatus::None,
            ml_score: None,
        }
    }

    fn assert_close(actual: Option<f32>, expected: f32) {
        let actual = actual.expect("a score");
        assert!((actual - expected).abs() < 1e-4, "{actual} != {expected}");
    }

    /// Hard-coded groups for a fixed fact set, regrouped as the analysis
    /// does (no ML, Stage 3 off, nothing manual, pinned or existing).
    #[test]
    fn matches_analyze_without_ml() {
        let facts = vec![
            // Bucket 0: a chain 0–5–10 links; 100 is far from it; 4 is unhashed.
            photo("a/0.jpg", 0),
            photo("a/1.jpg", 1),
            photo("a/2.jpg", 2),
            photo("a/3.jpg", 3),
            photo("a/4.jpg", 4),
            // Bucket 2: a bracket whose shots are 20 apart, linked by its run.
            // The ids are not in shot order, the layers are.
            bracket_shot("a/5.jpg", 22, 3),
            bracket_shot("a/6.jpg", 20, 1),
            bracket_shot("a/7.jpg", 21, 2),
            // Bucket 1: another body, interleaved with bucket 0.
            shot_on("Other", "a/8.jpg", 1),
            shot_on("Other", "a/9.jpg", 2),
            // No EXIF: one bucket for the folder.
            no_exif("b/y.jpg"),
            no_exif("b/x.jpg"),
        ];
        let hashes = [
            hash(0),
            hash(5),
            hash(10),
            hash(100),
            None,
            hash(40),
            hash(0),
            hash(20),
            hash(0),
            hash(1),
            hash(2),
            hash(0),
        ];
        let thresholds = Thresholds {
            floor: u32::MAX,
            ..THRESHOLDS
        };
        let regrouped = regroup(&RegroupInput {
            thresholds,
            ..input(&facts, &hashes)
        });
        assert_eq!(
            regrouped,
            Regrouped {
                groups: vec![
                    proposed(&[0, 1, 2], "a|Body|?|?@0", 10),
                    proposed(&[8, 9], "a|Other|?|?@1", 1),
                    proposed(&[6, 7, 5], "a|Body|?|?@2", 40),
                    proposed(&[11, 10], "b|no-exif", 2),
                ],
                unstacked: vec![3],
                needs_embeddings: vec![],
                removed: vec![],
            }
        );
    }

    #[test]
    fn unchanged_group_keeps_status_and_id() {
        let (facts, hashes) = then_burst(burst(0, &[0, 2]), &[0, 2]);
        let before = [
            existing(7, &[1, 0], GroupStatus::Confirmed),
            existing(9, &[3, 2], GroupStatus::Rejected),
        ];
        // A slider drag that changes no member set changes no review.
        for hamming in [8, 3] {
            let regrouped = regroup(&RegroupInput {
                thresholds: Thresholds {
                    hamming,
                    ..THRESHOLDS
                },
                existing: &before,
                ..input(&facts, &hashes)
            });
            let reviews: Vec<_> = regrouped
                .groups
                .iter()
                .map(|g| (g.id, g.members.clone(), g.status, g.review_reset))
                .collect();
            assert_eq!(
                reviews,
                [
                    (Some(7), vec![0, 1], GroupStatus::Confirmed, false),
                    (Some(9), vec![2, 3], GroupStatus::Rejected, false),
                ],
                "hamming {hamming}"
            );
            assert!(regrouped.removed.is_empty(), "{regrouped:?}");
        }
    }

    #[test]
    fn changed_group_is_proposed_with_review_reset() {
        // 2 is 18 from its neighbour: it leaves the confirmed group. 5 joins
        // the rejected one.
        let (facts, hashes) = then_burst(burst(0, &[0, 2, 20]), &[0, 2, 4]);
        let before = [
            existing(5, &[0, 1, 2], GroupStatus::Confirmed),
            existing(6, &[3, 4], GroupStatus::Rejected),
        ];
        let regrouped = regroup(&RegroupInput {
            existing: &before,
            ..input(&facts, &hashes)
        });
        let reviews: Vec<_> = regrouped
            .groups
            .iter()
            .map(|g| (g.id, g.members.clone(), g.status, g.review_reset))
            .collect();
        assert_eq!(
            reviews,
            [
                (None, vec![0, 1], GroupStatus::Proposed, true),
                (None, vec![3, 4, 5], GroupStatus::Proposed, true),
            ]
        );
        assert_eq!(regrouped.unstacked, [2]);
        assert_eq!(regrouped.removed, [5, 6]);
    }

    #[test]
    fn a_reused_proposed_group_keeps_its_review_flag() {
        // As `changed_group_is_proposed_with_review_reset` left them: two new
        // groups flagged, the second made of photos the user had rejected.
        let (facts, hashes) = then_burst(burst(0, &[0, 2, 20]), &[0, 2, 4]);
        let flagged = |id, members: &[usize]| ExistingGroup {
            review_reset: true,
            ..existing(id, members, GroupStatus::Proposed)
        };
        let before = [flagged(7, &[0, 1]), flagged(8, &[3, 4, 5])];
        let reviews = |regrouped: &Regrouped| -> Vec<_> {
            regrouped
                .groups
                .iter()
                .map(|g| (g.id, g.members.clone(), g.status, g.review_reset))
                .collect()
        };

        // The same member sets: the same groups, still to look at again.
        let regrouped = regroup(&RegroupInput {
            existing: &before,
            ..input(&facts, &hashes)
        });
        assert_eq!(
            reviews(&regrouped),
            [
                (Some(7), vec![0, 1], GroupStatus::Proposed, true),
                (Some(8), vec![3, 4, 5], GroupStatus::Proposed, true),
            ]
        );
        assert!(regrouped.removed.is_empty(), "{regrouped:?}");

        // Other members: a new group, which takes the flag of the flagged group
        // it replaces, however many regroupings it took to get there.
        let grown = |existing: &[ExistingGroup]| {
            regroup(&RegroupInput {
                thresholds: Thresholds {
                    hamming: 20,
                    ..THRESHOLDS
                },
                existing,
                ..input(&facts, &hashes)
            })
        };
        let regrouped = grown(&before);
        assert_eq!(
            reviews(&regrouped),
            [
                (None, vec![0, 1, 2], GroupStatus::Proposed, true),
                (Some(8), vec![3, 4, 5], GroupStatus::Proposed, true),
            ]
        );
        assert_eq!(regrouped.removed, [7]);

        // A proposed group without the flag passes none on.
        let unflagged = [
            existing(7, &[0, 1], GroupStatus::Proposed),
            flagged(8, &[3, 4, 5]),
        ];
        assert_eq!(
            reviews(&grown(&unflagged))[0],
            (None, vec![0, 1, 2], GroupStatus::Proposed, false)
        );
    }

    #[test]
    fn new_group_from_untouched_photos_has_no_review_reset() {
        let (facts, hashes) = then_burst(then_burst(burst(0, &[0, 2, 20]), &[0, 2]), &[0, 2]);
        let before = [
            // Changed, but nobody had decided on it.
            existing(1, &[0, 1, 2], GroupStatus::Proposed),
            // Unchanged: its decision does not reach the other groups.
            existing(2, &[5, 6], GroupStatus::Confirmed),
        ];
        let regrouped = regroup(&RegroupInput {
            existing: &before,
            ..input(&facts, &hashes)
        });
        let reviews: Vec<_> = regrouped
            .groups
            .iter()
            .map(|g| (g.id, g.members.clone(), g.status, g.review_reset))
            .collect();
        assert_eq!(
            reviews,
            [
                (None, vec![0, 1], GroupStatus::Proposed, false),
                // 3 and 4 were in no group.
                (None, vec![3, 4], GroupStatus::Proposed, false),
                (Some(2), vec![5, 6], GroupStatus::Confirmed, false),
            ]
        );
        assert_eq!(regrouped.removed, [1]);
    }

    #[test]
    fn manual_groups_are_returned_unchanged() {
        // 0, 1 and 2 would link; 3 links with nothing.
        let (facts, hashes) = burst(0, &[0, 0, 0, 100]);
        let manual = [ManualGroup {
            id: 3,
            members: vec![3, 0],
        }];
        let before = [existing(8, &[0, 1, 2], GroupStatus::Proposed)];
        let regrouped = regroup(&RegroupInput {
            manual: &manual,
            existing: &before,
            ..input(&facts, &hashes)
        });
        assert_eq!(members(&regrouped), [vec![1, 2]]);
        // A manual member is not in the tray either, even when it links with nothing.
        assert!(regrouped.unstacked.is_empty(), "{regrouped:?}");
        assert_eq!(regrouped.removed, [8]);
    }

    #[test]
    fn pinned_photo_stays_in_the_tray() {
        let (mut facts, mut hashes) = burst(0, &[0, 0, 0]);
        // An unhashed photo is never in the tray, pinned or not.
        facts.push(photo("a/3.jpg", 3));
        hashes.push(None);
        let regrouped = regroup(&RegroupInput {
            pinned: &[3, 1, 1],
            ..input(&facts, &hashes)
        });
        assert_eq!(members(&regrouped), [vec![0, 2]]);
        assert_eq!(regrouped.unstacked, [1]);
    }

    #[test]
    fn pinning_a_middle_shot_does_not_split_its_bucket() {
        // 0 s, 3 s and 6 s with a 4 s window: one bucket only through the
        // 3 s shot, since 0 s and 6 s are 6 s apart.
        let facts = vec![
            photo("a/0.jpg", 0),
            photo("a/1.jpg", 3),
            photo("a/2.jpg", 6),
        ];
        let hashes = [hash(0), hash(0), hash(0)];
        let unpinned = regroup(&input(&facts, &hashes));
        assert_eq!(members(&unpinned), [vec![0, 1, 2]]);

        let pinned = regroup(&RegroupInput {
            pinned: &[1],
            ..input(&facts, &hashes)
        });
        assert_eq!(members(&pinned), [vec![0, 2]]);
        assert_eq!(pinned.unstacked, [1]);
        assert_eq!(pinned.groups[0].bucket, unpinned.groups[0].bucket);

        // A manual group does not split the bucket either.
        let manual = [ManualGroup {
            id: 1,
            members: vec![1],
        }];
        let edited = regroup(&RegroupInput {
            manual: &manual,
            ..input(&facts, &hashes)
        });
        assert_eq!(members(&edited), [vec![0, 2]]);
        assert!(edited.unstacked.is_empty(), "{edited:?}");
        assert_eq!(edited.groups[0].bucket, unpinned.groups[0].bucket);
    }

    #[test]
    fn ambiguous_group_without_ml_is_skipped() {
        // 0–5–10 is above the floor of 6; 0–2 is not.
        let (facts, hashes) = then_burst(burst(0, &[0, 5, 10]), &[0, 2]);
        let regrouped = regroup(&input(&facts, &hashes));
        assert_eq!(members(&regrouped), [vec![0, 1, 2], vec![3, 4]]);
        assert_eq!(ml_statuses(&regrouped), [MlStatus::Skipped, MlStatus::None]);
        assert!(regrouped.groups.iter().all(|g| g.ml_score.is_none()));
        assert!(regrouped.needs_embeddings.is_empty(), "{regrouped:?}");
    }

    #[test]
    fn missing_embedding_makes_the_group_pending() {
        let (facts, hashes) = then_burst(then_burst(burst(0, &[0, 5, 10]), &[0, 2]), &[0]);
        // 1 has no embedding; nor have the unambiguous group and the singleton.
        let table = [unit(0, 2), None, unit(1, 2), None, None, None];
        let embed = |id| embedding(&table, id);
        let regrouped = regroup(&RegroupInput {
            ml: Some(&embed),
            ..input(&facts, &hashes)
        });
        // The group stays whole until the embedding arrives.
        assert_eq!(members(&regrouped), [vec![0, 1, 2], vec![3, 4]]);
        assert_eq!(ml_statuses(&regrouped), [MlStatus::Pending, MlStatus::None]);
        assert_eq!(regrouped.groups[0].ml_score, None);
        assert_eq!(regrouped.needs_embeddings, [1]);
        assert_eq!(regrouped.unstacked, [5]);
    }

    #[test]
    fn cosine_split_gives_split_groups_and_tray_singletons() {
        // One Stage-2 chain, 20 from end to end.
        let (facts, hashes) = burst(0, &[0, 5, 10, 15, 20]);
        let table = [
            unit(0, 3),
            // 0.9 from unit 0.
            Some(vec![0.9, 0.19_f32.sqrt(), 0.0]),
            unit(1, 3),
            unit(1, 3),
            unit(2, 3),
        ];
        let embed = |id| embedding(&table, id);
        let regrouped = regroup(&RegroupInput {
            ml: Some(&embed),
            ..input(&facts, &hashes)
        });
        assert_eq!(members(&regrouped), [vec![0, 1], vec![2, 3]]);
        assert_eq!(ml_statuses(&regrouped), [MlStatus::Split, MlStatus::Split]);
        assert_close(regrouped.groups[0].ml_score, 0.9);
        assert_close(regrouped.groups[1].ml_score, 1.0);
        // Each part has its own maximum distance and keeps the bucket.
        let distances: Vec<_> = regrouped.groups.iter().map(|g| g.max_distance).collect();
        assert_eq!(distances, [5, 5]);
        assert!(regrouped.groups.iter().all(|g| g.bucket == "a|Body|?|?@0"));
        assert_eq!(regrouped.unstacked, [4]);
    }

    #[test]
    fn ambiguous_group_with_similar_embeddings_is_confirmed() {
        let (facts, hashes) = burst(0, &[0, 5, 10]);
        let close = Some(vec![0.9, 0.19_f32.sqrt()]);
        let table = [unit(0, 2), unit(0, 2), close];
        let embed = |id| embedding(&table, id);
        let regrouped = regroup(&RegroupInput {
            ml: Some(&embed),
            ..input(&facts, &hashes)
        });
        assert_eq!(members(&regrouped), [vec![0, 1, 2]]);
        assert_eq!(ml_statuses(&regrouped), [MlStatus::Confirmed]);
        // The mean of 1.0, 0.9 and 0.9.
        assert_close(regrouped.groups[0].ml_score, 2.8 / 3.0);
    }

    #[test]
    fn bracket_run_is_never_split_by_cosine() {
        // A ±6 EV bracket of five: 10 apart shot to shot, 40 end to end, and
        // no two shots alike to the embedding (cosine 0.0 everywhere).
        let facts: Vec<FileFacts> = (0..5)
            .map(|i| bracket_shot(&format!("a/{i}.jpg"), i as i64, i as u16 + 1))
            .collect();
        let hashes: Vec<_> = (0..5).map(|i| hash(10 * i)).collect();
        let table: Vec<_> = (0..6).map(|i| unit(i, 6)).collect();
        let embed = |id| embedding(&table, id);
        let regrouped = regroup(&RegroupInput {
            ml: Some(&embed),
            ..input(&facts, &hashes)
        });
        assert_eq!(members(&regrouped), [vec![0, 1, 2, 3, 4]]);
        assert_eq!(regrouped.groups[0].max_distance, 40);
        assert_eq!(ml_statuses(&regrouped), [MlStatus::Confirmed]);
        assert_close(regrouped.groups[0].ml_score, 0.0);

        // A photo outside the run that Stage 2 linked by its hash is still
        // checked: it splits off, and the bracket stays whole.
        let mut extra_facts = facts.clone();
        extra_facts.push(photo("a/5.jpg", 5));
        let mut extra_hashes = hashes.clone();
        extra_hashes.push(hash(42));
        let regrouped = regroup(&RegroupInput {
            ml: Some(&embed),
            ..input(&extra_facts, &extra_hashes)
        });
        assert_eq!(members(&regrouped), [vec![0, 1, 2, 3, 4]]);
        assert_eq!(ml_statuses(&regrouped), [MlStatus::Split]);
        assert_eq!(regrouped.unstacked, [5]);
    }

    #[test]
    fn group_at_or_below_the_floor_is_not_checked() {
        // 0–3–6 has a maximum of 6, the floor; 0–4–7 has 7.
        let (facts, hashes) = then_burst(burst(0, &[0, 3, 6]), &[0, 4, 7]);
        let table: [Option<Vec<f32>>; 0] = [];
        let embed = |id| embedding(&table, id);
        let regrouped = regroup(&RegroupInput {
            ml: Some(&embed),
            ..input(&facts, &hashes)
        });
        assert_eq!(members(&regrouped), [vec![0, 1, 2], vec![3, 4, 5]]);
        assert_eq!(ml_statuses(&regrouped), [MlStatus::None, MlStatus::Pending]);
        // Only the checked group asks for embeddings.
        assert_eq!(regrouped.needs_embeddings, [3, 4, 5]);
    }

    /// A job with a bit of everything: a cosine split, a pinned photo, a
    /// bracket held by its run, a manual group, a no-EXIF group, a singleton,
    /// an unhashed photo, and reviews to match.
    struct Scene {
        facts: Vec<FileFacts>,
        hashes: Vec<Option<HashPair>>,
        table: Vec<Option<Vec<f32>>>,
        manual: Vec<ManualGroup>,
        pinned: Vec<usize>,
        existing: Vec<ExistingGroup>,
    }

    fn scene() -> Scene {
        let facts = vec![
            photo("a/0.jpg", 0),
            photo("a/1.jpg", 1),
            photo("a/2.jpg", 2),
            photo("a/3.jpg", 3),
            photo("a/4.jpg", 2),
            bracket_shot("a/5.jpg", 20, 1),
            bracket_shot("a/6.jpg", 21, 2),
            bracket_shot("a/7.jpg", 22, 3),
            photo("a/8.jpg", 23),
            photo("a/9.jpg", 24),
            no_exif("b/10.jpg"),
            no_exif("b/11.jpg"),
            photo("a/12.jpg", 40),
            photo("a/13.jpg", 41),
        ];
        let hashes = vec![
            hash(0),
            hash(5),
            hash(10),
            hash(15),
            hash(5),
            hash(0),
            hash(20),
            hash(40),
            hash(40),
            hash(40),
            hash(0),
            hash(1),
            hash(0),
            None,
        ];
        let mut table = vec![
            unit(0, 4),
            unit(0, 4),
            unit(1, 4),
            unit(1, 4),
            None,
            unit(2, 4),
            unit(3, 4),
            unit(0, 4),
        ];
        table.resize(facts.len(), None);
        Scene {
            facts,
            hashes,
            table,
            manual: vec![ManualGroup {
                id: 200,
                members: vec![9, 8],
            }],
            pinned: vec![4],
            existing: vec![
                existing(100, &[7, 6, 5], GroupStatus::Confirmed),
                existing(101, &[0, 1, 2, 3], GroupStatus::Confirmed),
                existing(102, &[11, 10], GroupStatus::Rejected),
                existing(103, &[8, 9], GroupStatus::Proposed),
            ],
        }
    }

    fn regroup_scene(scene: &Scene) -> Regrouped {
        let embed = |id| embedding(&scene.table, id);
        regroup(&RegroupInput {
            facts: &scene.facts,
            hashes: &scene.hashes,
            thresholds: THRESHOLDS,
            ml: Some(&embed),
            manual: &scene.manual,
            pinned: &scene.pinned,
            existing: &scene.existing,
        })
    }

    /// The scene with file `old` renumbered `to[old]`, and every list
    /// reversed.
    fn renumbered(scene: &Scene, to: &[usize]) -> Scene {
        fn place<T: Clone>(items: &[T], to: &[usize]) -> Vec<T> {
            let mut placed = items.to_vec();
            for (old, item) in items.iter().enumerate() {
                placed[to[old]] = item.clone();
            }
            placed
        }
        let ids = |members: &[usize]| members.iter().rev().map(|&id| to[id]).collect();
        Scene {
            facts: place(&scene.facts, to),
            hashes: place(&scene.hashes, to),
            table: place(&scene.table, to),
            manual: scene
                .manual
                .iter()
                .rev()
                .map(|g| ManualGroup {
                    id: g.id,
                    members: ids(&g.members),
                })
                .collect(),
            pinned: ids(&scene.pinned),
            existing: scene
                .existing
                .iter()
                .rev()
                .map(|g| ExistingGroup {
                    members: ids(&g.members),
                    ..g.clone()
                })
                .collect(),
        }
    }

    type GroupByPath = (
        Option<i64>,
        Vec<PathBuf>,
        String,
        u32,
        GroupStatus,
        bool,
        MlStatus,
        Option<f32>,
    );

    /// The result with file ids spelled as paths, groups sorted by their
    /// first path, since a renumbering may reorder groups of one bucket.
    fn by_path(
        scene: &Scene,
        regrouped: &Regrouped,
    ) -> (Vec<GroupByPath>, Vec<PathBuf>, Vec<PathBuf>, Vec<i64>) {
        let paths = |ids: &[usize]| -> Vec<PathBuf> {
            ids.iter().map(|&id| scene.facts[id].path.clone()).collect()
        };
        let sorted_paths = |ids: &[usize]| {
            let mut paths = paths(ids);
            paths.sort();
            paths
        };
        let mut groups: Vec<GroupByPath> = regrouped
            .groups
            .iter()
            .map(|g| {
                (
                    g.id,
                    paths(&g.members),
                    g.bucket.clone(),
                    g.max_distance,
                    g.status,
                    g.review_reset,
                    g.ml_status,
                    g.ml_score,
                )
            })
            .collect();
        groups.sort_by(|a, b| a.1.cmp(&b.1));
        (
            groups,
            sorted_paths(&regrouped.unstacked),
            sorted_paths(&regrouped.needs_embeddings),
            regrouped.removed.clone(),
        )
    }

    #[test]
    fn regroup_is_independent_of_input_order() {
        let base = scene();
        let expected = regroup_scene(&base);
        // The scene is not trivial.
        assert_eq!(
            members(&expected),
            [vec![0, 1], vec![2, 3], vec![5, 6, 7], vec![10, 11]]
        );
        let reviews: Vec<_> = expected
            .groups
            .iter()
            .map(|g| (g.id, g.status, g.review_reset, g.ml_status))
            .collect();
        assert_eq!(
            reviews,
            [
                (None, GroupStatus::Proposed, true, MlStatus::Split),
                (None, GroupStatus::Proposed, true, MlStatus::Split),
                (
                    Some(100),
                    GroupStatus::Confirmed,
                    false,
                    MlStatus::Confirmed
                ),
                (Some(102), GroupStatus::Rejected, false, MlStatus::None),
            ]
        );
        assert_eq!(expected.unstacked, [4, 12]);
        assert!(expected.needs_embeddings.is_empty(), "{expected:?}");
        assert_eq!(expected.removed, [101, 103]);

        // Only the lists reversed: the very same result.
        let identity: Vec<usize> = (0..base.facts.len()).collect();
        assert_eq!(regroup_scene(&renumbered(&base, &identity)), expected);

        // Renumbered files: the same result, file for file.
        let n = base.facts.len();
        let reversed: Vec<usize> = (0..n).rev().collect();
        let rotated: Vec<usize> = (0..n).map(|id| (id + 5) % n).collect();
        for to in [reversed, rotated] {
            let scene = renumbered(&base, &to);
            assert_eq!(
                by_path(&scene, &regroup_scene(&scene)),
                by_path(&base, &expected),
                "renumbered {to:?}"
            );
        }
    }
}
