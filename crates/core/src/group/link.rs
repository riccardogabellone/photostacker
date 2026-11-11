//! Stage 2: links the photos of one bucket into stacks by perceptual-hash
//! distance (spec §5.3).
//!
//! Every pair of hashed members within the threshold is linked, and so is
//! every extra edge the caller passes (the bracket-run links), and the stacks
//! are the connected components of those links. Unlike comparing each photo
//! with a group's first member, this depends only on the set of members and
//! edges, never on their order: a chain A–B–C stays one stack even when A and
//! C are too far apart to link directly.

use crate::hash::{HashPair, distance};

/// The partition of one bucket's hashed members. Members without a hash
/// appear in neither list.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Linked {
    /// Stacks of at least two ids, each ascending, ordered by their smallest id.
    pub groups: Vec<Vec<usize>>,
    /// Ids that linked with nothing, ascending.
    pub singletons: Vec<usize>,
}

/// Links the `members` (indices into `hashes`, in any order) whose combined
/// hash distance is at most `threshold`, links the two ends of each `extra`
/// edge whatever their distance, and returns the connected components. An
/// extra edge with an end that is not a hashed member is ignored.
///
/// # Panics
///
/// If a member is not a valid index into `hashes`.
pub fn link(
    members: &[usize],
    hashes: &[Option<HashPair>],
    threshold: u32,
    extra: &[(usize, usize)],
) -> Linked {
    let mut hashed: Vec<(usize, HashPair)> = members
        .iter()
        .filter_map(|&id| hashes[id].map(|hash| (id, hash)))
        .collect();
    // Sorting fixes the union-find's input, so its result is the same for
    // every order of `members`; dedup keeps a repeated id from linking to itself.
    hashed.sort_by_key(|&(id, _)| id);
    hashed.dedup_by_key(|&mut (id, _)| id);

    let mut components = UnionFind::new(hashed.len());
    for (i, &(_, a)) in hashed.iter().enumerate() {
        for (j, &(_, b)) in hashed.iter().enumerate().skip(i + 1) {
            if distance(a, b) <= threshold {
                components.union(i, j);
            }
        }
    }
    let index_of = |id: usize| hashed.binary_search_by_key(&id, |&(id, _)| id).ok();
    for &(a, b) in extra {
        if let (Some(i), Some(j)) = (index_of(a), index_of(b)) {
            components.union(i, j);
        }
    }

    let mut linked = Linked::default();
    for component in components.components() {
        // Indices into `hashed`, which is ascending by id: the ids are too.
        let component: Vec<usize> = component.into_iter().map(|i| hashed[i].0).collect();
        match component.as_slice() {
            [id] => linked.singletons.push(*id),
            _ => linked.groups.push(component),
        }
    }
    linked.groups.sort_by_key(|group| group[0]);
    linked.singletons.sort_unstable();
    linked
}

/// The largest combined hash distance between two hashed members of `group`;
/// 0 when fewer than two members have a hash.
///
/// # Panics
///
/// If a member of `group` is not a valid index into `hashes`.
pub fn max_pairwise_distance(group: &[usize], hashes: &[Option<HashPair>]) -> u32 {
    let hashed: Vec<HashPair> = group.iter().filter_map(|&id| hashes[id]).collect();
    hashed
        .iter()
        .enumerate()
        .flat_map(|(i, &a)| hashed[i + 1..].iter().map(move |&b| distance(a, b)))
        .max()
        .unwrap_or(0)
}

/// Disjoint sets over `0..n`, with path halving.
pub(super) struct UnionFind {
    parent: Vec<usize>,
}

impl UnionFind {
    pub(super) fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
        }
    }

    fn find(&mut self, mut x: usize) -> usize {
        while self.parent[x] != x {
            self.parent[x] = self.parent[self.parent[x]];
            x = self.parent[x];
        }
        x
    }

    pub(super) fn union(&mut self, a: usize, b: usize) {
        let (a, b) = (self.find(a), self.find(b));
        // The smaller index becomes the root, so the outcome is deterministic.
        let (root, child) = if a <= b { (a, b) } else { (b, a) };
        self.parent[child] = root;
    }

    /// The sets, each ascending, ordered by their smallest element.
    pub(super) fn components(&mut self) -> Vec<Vec<usize>> {
        // A set's root is its smallest element, and walking in ascending
        // order keeps every set ascending.
        let mut by_root: Vec<Vec<usize>> = vec![Vec::new(); self.parent.len()];
        for x in 0..self.parent.len() {
            let root = self.find(x);
            by_root[root].push(x);
        }
        by_root.retain(|set| !set.is_empty());
        by_root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const THRESHOLD: u32 = 8;

    fn hash(phash: u64) -> Option<HashPair> {
        Some(HashPair { phash, dhash: 0 })
    }

    /// Three hashes in a chain: A–B and B–C are 6 apart, A–C is 12.
    const A: u64 = 0;
    const B: u64 = 0x3F;
    const C: u64 = 0xFFF;
    /// 64 bits from A, B and C alike: links with nothing.
    const FAR: u64 = !0;

    #[test]
    fn chain_links_transitively() {
        let hashes = [hash(A), hash(B), hash(C)];
        let linked = link(&[0, 1, 2], &hashes, THRESHOLD, &[]);
        assert_eq!(linked.groups, [vec![0, 1, 2]]);
        assert!(linked.singletons.is_empty(), "{linked:?}");

        // The threshold is inclusive.
        assert_eq!(link(&[0, 1, 2], &hashes, 6, &[]).groups, [vec![0, 1, 2]]);
        let below = link(&[0, 1, 2], &hashes, 5, &[]);
        assert!(below.groups.is_empty(), "{below:?}");
        assert_eq!(below.singletons, [0, 1, 2]);
    }

    #[test]
    fn link_is_order_independent() {
        // Sparse ids into a longer hash table. Linking a photo to a group's
        // first member only would split the chain for some orders.
        let hashes = [hash(FAR), hash(A), None, hash(B), hash(C)];
        let permutations = [
            [1, 3, 4],
            [1, 4, 3],
            [3, 1, 4],
            [3, 4, 1],
            [4, 1, 3],
            [4, 3, 1],
        ];
        for members in permutations {
            assert_eq!(
                link(&members, &hashes, THRESHOLD, &[]),
                Linked {
                    groups: vec![vec![1, 3, 4]],
                    singletons: vec![],
                },
                "members {members:?}"
            );
        }
    }

    #[test]
    fn singletons_are_returned_not_dropped() {
        // 3 and 5 are at least 16 from everything else and from each other.
        let hashes = [
            hash(A),
            hash(FAR),
            hash(B),
            hash(0xFFFF_0000),
            hash(FAR),
            hash(0xFFFF_0000_0000),
        ];
        let linked = link(&[5, 4, 3, 1, 2, 0], &hashes, THRESHOLD, &[]);
        assert_eq!(
            linked,
            Linked {
                groups: vec![vec![0, 2], vec![1, 4]],
                singletons: vec![3, 5],
            }
        );

        let apart = [hash(A), hash(FAR), hash(B ^ 0xFF00)];
        let linked = link(&[2, 1, 0], &apart, THRESHOLD, &[]);
        assert!(linked.groups.is_empty(), "{linked:?}");
        assert_eq!(linked.singletons, [0, 1, 2]);
    }

    #[test]
    fn groups_are_sorted_by_smallest_id() {
        let hashes = [hash(FAR), hash(A), hash(FAR), hash(B), hash(FAR)];
        let linked = link(&[3, 4, 2, 1, 0], &hashes, THRESHOLD, &[]);
        assert_eq!(linked.groups, [vec![0, 2, 4], vec![1, 3]]);
    }

    #[test]
    fn unhashed_members_are_ignored() {
        let hashes = [hash(A), None, hash(B), None];
        let linked = link(&[3, 2, 1, 0], &hashes, THRESHOLD, &[]);
        assert_eq!(
            linked,
            Linked {
                groups: vec![vec![0, 2]],
                singletons: vec![],
            }
        );
        // An unhashed member is not a singleton either.
        assert_eq!(link(&[1], &hashes, THRESHOLD, &[]), Linked::default());
    }

    #[test]
    fn extra_edges_link_beyond_the_threshold() {
        // A–B–C chain by distance; FAR links with nothing by distance.
        let hashes = [hash(A), hash(FAR), hash(C), hash(B), hash(FAR), None];
        let members = [0, 1, 2, 3, 5];
        let without = link(&members, &hashes, THRESHOLD, &[]);
        assert_eq!(without.groups, [vec![0, 2, 3]]);
        assert_eq!(without.singletons, [1]);

        // The extra edge joins FAR to the chain, in either direction.
        for edge in [(2, 1), (1, 2)] {
            let linked = link(&members, &hashes, THRESHOLD, &[edge]);
            assert_eq!(
                linked,
                Linked {
                    groups: vec![vec![0, 1, 2, 3]],
                    singletons: vec![],
                },
                "edge {edge:?}"
            );
        }
        // An edge to a non-member (4) or to an unhashed member (5) is ignored.
        let ignored = link(&members, &hashes, THRESHOLD, &[(1, 4), (5, 1), (4, 5)]);
        assert_eq!(ignored, without);
    }

    #[test]
    fn max_pairwise_distance_skips_unhashed() {
        let hashes = [hash(A), hash(B), None, hash(C)];
        assert_eq!(max_pairwise_distance(&[0, 1, 2, 3], &hashes), 12);
        assert_eq!(max_pairwise_distance(&[1, 3], &hashes), 6);
        assert_eq!(max_pairwise_distance(&[3, 2], &hashes), 0);
        assert_eq!(max_pairwise_distance(&[0], &hashes), 0);
        assert_eq!(max_pairwise_distance(&[], &hashes), 0);
    }
}
