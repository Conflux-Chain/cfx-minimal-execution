//! Two-level entry store for the incremental trie.
//!
//! Entry keys share long prefixes — the survey found 169M of 250M snapshot keys
//! are 57-byte storage keys, most sharing a 20-byte account prefix with many
//! siblings. Storing each full key independently repeats that prefix millions of
//! times. This store factors it out:
//!
//! * **Level 1** is a `BTreeMap` keyed by [`Prefix23`] — the first ≤23 bytes of
//!   the key, packed into a 24-byte inline value (`[u8; 23]` data + a length
//!   byte). The length byte as the least-significant field makes the derived
//!   `Ord` match slice-lexicographic order exactly (a shorter key sorts before
//!   its extensions), so the map iterates in true key order.
//! * A level-1 value is either a **`Leaf(V)`** — for keys ≤22 bytes, which end
//!   inside the 23-byte prefix — or a **`Branch`**: a second `BTreeMap` keyed by
//!   the key *remainder* (`key[23..]`) as a `SmallVec<u8, 40>` (48 B, inline up
//!   to 40 B — covers the 33/34-byte tails of the dominant keys with no heap
//!   allocation). The leaf-vs-branch split is by length at runtime (a key of
//!   ≤22 B can never collide with a 23-byte branch head), so the wrong-variant
//!   arms are `unreachable`.
//!
//! Flattened (level 1 in order, each branch's level 2 in order) this yields the
//! exact same sorted `(full key, V)` sequence as a flat `BTreeMap<Box<[u8]>, V>`,
//! but without the per-entry prefix duplication or per-key heap allocation.
//! Keys are surfaced to the trie walk as a [`Composite`] (`head` + `tail` slices)
//! read through [`Nibbles::from_composite`] — never reassembled into one buffer.

use crate::nibbles::Nibbles;
use smallvec::SmallVec;
use std::cmp::Ordering;
use std::collections::{btree_map, BTreeMap};
use std::ops::Bound;

/// Bytes of a key held in the level-1 key. Keys this long or shorter are leaves;
/// longer keys become branch heads carrying exactly this many prefix bytes.
const HEAD_LEN: usize = 23;
/// Maximum key byte length stored directly as a `Leaf` (fits in `HEAD_LEN` with
/// room for a distinct length byte, so it never collides with a branch head).
const LEAF_MAX: usize = HEAD_LEN - 1;
/// Inline capacity of the level-2 key (48-byte `SmallVec`; see module docs).
const TAIL_INLINE: usize = 40;

type Level2Key = SmallVec<u8, TAIL_INLINE>;

fn level2_key(bytes: &[u8]) -> Level2Key {
    let mut v = Level2Key::new();
    v.extend_from_slice(bytes);
    v
}

/// A level-1 key: up to [`HEAD_LEN`] key bytes plus their length. The length as
/// the last (least-significant) field makes `#[derive(Ord)]` reproduce
/// slice-lexicographic order (zero-padded data compares first, then a shorter
/// key — a prefix — sorts before its longer extensions).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) struct Prefix23 {
    data: [u8; HEAD_LEN],
    len: u8,
}

impl Prefix23 {
    fn new(bytes: &[u8]) -> Self {
        debug_assert!(bytes.len() <= HEAD_LEN);
        let mut data = [0u8; HEAD_LEN];
        data[..bytes.len()].copy_from_slice(bytes);
        Self {
            data,
            len: bytes.len() as u8,
        }
    }

    #[inline]
    fn as_slice(&self) -> &[u8] {
        &self.data[..self.len as usize]
    }
}

// `Borrow<[u8]>` (order-consistent with the derived `Ord`, verified for all
// lengths) lets level-1 lookups and ranges use a `&[u8]` slice directly — no
// throwaway `Prefix23` construction for reads.
impl std::borrow::Borrow<[u8]> for Prefix23 {
    fn borrow(&self) -> &[u8] {
        self.as_slice()
    }
}

impl AsRef<[u8]> for Prefix23 {
    fn as_ref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl PartialEq<[u8]> for Prefix23 {
    fn eq(&self, other: &[u8]) -> bool {
        self.as_slice() == other
    }
}

enum Node<V> {
    Leaf(V),
    Branch(BTreeMap<Level2Key, V>),
}

/// The level-1 key of a canonical key: its first ≤23 bytes (a ≤22B key is its
/// own whole leaf prefix; a ≥23B key contributes its first 23 bytes).
fn prefix23_of(key: &[u8]) -> Prefix23 {
    Prefix23::new(&key[..key.len().min(HEAD_LEN)])
}

/// A key surfaced from the store as `head ++ tail`, without reassembly. For a
/// leaf `tail` is empty; for a branch entry `head` is the 23-byte prefix and
/// `tail` the remainder.
#[derive(Clone, Copy)]
pub(crate) struct Composite<'a> {
    pub(crate) head: &'a [u8],
    pub(crate) tail: &'a [u8],
}

impl<'a> Composite<'a> {
    #[inline]
    pub(crate) fn nibbles(&self) -> Nibbles<'a> {
        Nibbles::from_composite(self.head, self.tail)
    }

    #[inline]
    pub(crate) fn byte_len(&self) -> usize {
        self.head.len() + self.tail.len()
    }

    /// Whether two composites denote the same full key. Cheap: for entries from
    /// the same store, equal `head` slices share the same level-1 node, so this
    /// is a length-guarded slice compare, not a reassembly.
    #[inline]
    pub(crate) fn same_key(&self, other: &Composite<'_>) -> bool {
        self.head == other.head && self.tail == other.tail
    }
}

/// Compare `head ++ tail` against a contiguous slice `other`, without building
/// the concatenation.
fn cmp_composite(head: &[u8], tail: &[u8], other: &[u8]) -> Ordering {
    let hn = head.len();
    let m = hn.min(other.len());
    match head[..m].cmp(&other[..m]) {
        Ordering::Equal => {}
        ne => return ne,
    }
    if other.len() <= hn {
        // `other` lies within `head`; the composite is a (possibly proper)
        // extension of it.
        if hn == other.len() && tail.is_empty() {
            Ordering::Equal
        } else {
            Ordering::Greater
        }
    } else {
        tail.cmp(&other[hn..])
    }
}

/// Is a full key strictly/inclusively below the upper bound `hi`?
fn below_hi(head: &[u8], tail: &[u8], hi: Bound<&[u8]>) -> bool {
    match hi {
        Bound::Unbounded => true,
        Bound::Excluded(h) => cmp_composite(head, tail, h) == Ordering::Less,
        Bound::Included(h) => cmp_composite(head, tail, h) != Ordering::Greater,
    }
}

/// The [`Entries`] store: a level-1 `BTreeMap` of [`Prefix23`] → [`Node`].
pub(crate) struct Entries<V> {
    map: BTreeMap<Prefix23, Node<V>>,
    len: usize,
}

impl<V> Default for Entries<V> {
    fn default() -> Self {
        Self::new()
    }
}

// Manual Clone/Debug so callers don't need `V: Debug` etc. beyond what the trie
// already requires (the trie derives Clone/Debug on `IncrementalTrie<V>`).
impl<V: Clone> Clone for Entries<V> {
    fn clone(&self) -> Self {
        let map = self
            .map
            .iter()
            .map(|(k, node)| {
                let node = match node {
                    Node::Leaf(v) => Node::Leaf(v.clone()),
                    Node::Branch(b) => Node::Branch(b.clone()),
                };
                (*k, node)
            })
            .collect();
        Self { map, len: self.len }
    }
}

impl<V> std::fmt::Debug for Entries<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Entries")
            .field("len", &self.len)
            .field("level1_nodes", &self.map.len())
            .finish()
    }
}

impl<V> Entries<V> {
    pub(crate) fn new() -> Self {
        Self {
            map: BTreeMap::new(),
            len: 0,
        }
    }

    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// Number of level-1 nodes (distinct 23-byte prefixes). If this ≈ total
    /// entries, level-2 maps are ~singletons and the fill waste lives in level-1.
    #[cfg(feature = "mem-survey")]
    pub(crate) fn level1_len(&self) -> usize {
        self.map.len()
    }

    /// Histogram of level-2 group sizes (1,2,3,4,5-8,9-16,17-32,33-64,65-256,
    /// 257-1024,1025-4096,4097+). A leaf counts as size 1.
    #[cfg(feature = "mem-survey")]
    pub(crate) fn group_size_hist(&self) -> [u64; 12] {
        let mut h = [0u64; 12];
        for node in self.map.values() {
            let s = match node {
                Node::Leaf(_) => 1,
                Node::Branch(b) => b.len(),
            };
            let idx = match s {
                1 => 0, 2 => 1, 3 => 2, 4 => 3, 5..=8 => 4, 9..=16 => 5, 17..=32 => 6,
                33..=64 => 7, 65..=256 => 8, 257..=1024 => 9, 1025..=4096 => 10, _ => 11,
            };
            h[idx] += 1;
        }
        h
    }

    #[inline]
    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub(crate) fn clear(&mut self) {
        self.map.clear();
        self.len = 0;
    }

    pub(crate) fn insert(&mut self, key: &[u8], value: V) {
        if key.len() <= LEAF_MAX {
            if self.map.insert(Prefix23::new(key), Node::Leaf(value)).is_none() {
                self.len += 1;
            }
        } else {
            let node = self
                .map
                .entry(Prefix23::new(&key[..HEAD_LEN]))
                .or_insert_with(|| Node::Branch(BTreeMap::new()));
            match node {
                Node::Branch(b) => {
                    if b.insert(level2_key(&key[HEAD_LEN..]), value).is_none() {
                        self.len += 1;
                    }
                }
                Node::Leaf(_) => unreachable!("leaf/branch prefix collision"),
            }
        }
    }

    /// Extend with an **ascending** batch whose keys are all greater than any
    /// key already present (checkpoint load feeds fixed-size sorted batches).
    /// Each contiguous same-prefix run builds a dense level-2 map via
    /// `FromIterator` (full BTree leaves) instead of monotonic `insert` (which
    /// freezes split leaves ~half full). A run continuing a prefix from a prior
    /// batch is `append`-merged into its existing level-2. Level-1 stays plain
    /// `insert` (its fill isn't the bottleneck — 99.9% of groups are singletons).
    /// Larger batches split fewer big-contract runs → denser level-2.
    pub(crate) fn bulk_extend_sorted(&mut self, batch: Vec<(Vec<u8>, V)>) {
        use itertools::Itertools;
        let runs = batch.into_iter().chunk_by(|(k, _)| prefix23_of(k));
        for (prefix, run) in &runs {
            let mut run = run.peekable();
            if run.peek().is_some_and(|(k, _)| k.len() <= LEAF_MAX) {
                let (_, v) = run.next().expect("non-empty run");
                if self.map.insert(prefix, Node::Leaf(v)).is_none() {
                    self.len += 1;
                }
            } else {
                let mut chunk: BTreeMap<Level2Key, V> =
                    run.map(|(k, v)| (level2_key(&k[HEAD_LEN..]), v)).collect();
                self.len += chunk.len();
                match self
                    .map
                    .entry(prefix)
                    .or_insert_with(|| Node::Branch(BTreeMap::new()))
                {
                    Node::Branch(b) if b.is_empty() => *b = chunk,
                    Node::Branch(b) => b.append(&mut chunk),
                    Node::Leaf(_) => unreachable!("leaf/branch prefix collision"),
                }
            }
        }
    }

    pub(crate) fn remove(&mut self, key: &[u8]) {
        if key.len() <= LEAF_MAX {
            if self.map.remove(key).is_some() {
                self.len -= 1;
            }
        } else if let Some(Node::Branch(b)) = self.map.get_mut(&key[..HEAD_LEN]) {
            if b.remove(&key[HEAD_LEN..]).is_some() {
                self.len -= 1;
            }
            if b.is_empty() {
                self.map.remove(&key[..HEAD_LEN]);
            }
        }
    }

    pub(crate) fn get(&self, key: &[u8]) -> Option<&V> {
        if key.len() <= LEAF_MAX {
            match self.map.get(key)? {
                Node::Leaf(v) => Some(v),
                Node::Branch(_) => None,
            }
        } else {
            match self.map.get(&key[..HEAD_LEN])? {
                Node::Branch(b) => b.get(&key[HEAD_LEN..]),
                Node::Leaf(_) => None,
            }
        }
    }

    /// Forward iterator over `[lo, hi)` in sorted key order. The yielded
    /// [`Composite`]s borrow the store (`'a`); the bounds only need to outlive
    /// the walk (`'b`), so an endpoint can be returned past the bounds' scope.
    pub(crate) fn range<'a, 'b>(
        &'a self,
        lo: &'b [u8],
        hi: Bound<&'b [u8]>,
    ) -> RangeFwd<'a, 'b, V> {
        let l1_lo = &lo[..lo.len().min(HEAD_LEN)];
        RangeFwd {
            l1: self.map.range::<[u8], _>((Bound::Included(l1_lo), Bound::Unbounded)),
            branch: None,
            lo,
            hi,
            done: false,
        }
    }

    /// Full iterator over all entries in sorted key order.
    pub(crate) fn iter(&self) -> RangeFwd<'_, 'static, V> {
        self.range(&[], Bound::Unbounded)
    }

    /// First key in `[lo, hi)`, or `None` if the range is empty.
    pub(crate) fn range_first<'a>(&'a self, lo: &[u8], hi: Bound<&[u8]>) -> Option<Composite<'a>> {
        self.range(lo, hi).next().map(|(c, _)| c)
    }

    /// Last key in `[lo, hi)`, or `None` if empty. Both the level-1 reverse scan
    /// and the boundary branch's level-2 scan are **bounded by `hi`** (seeked,
    /// not linearly skipped from the global maximum), so this costs `O(log n)`
    /// per level — not `O(nodes above hi)`.
    pub(crate) fn range_last<'a>(&'a self, lo: &[u8], hi: Bound<&[u8]>) -> Option<Composite<'a>> {
        let l1_lo = &lo[..lo.len().min(HEAD_LEN)];
        // Upper-bound the level-1 range by hi's prefix so `next_back` starts just
        // below hi rather than at the map's global maximum.
        let l1_hi: Bound<&[u8]> = match hi {
            Bound::Unbounded => Bound::Unbounded,
            Bound::Excluded(h) | Bound::Included(h) => {
                Bound::Included(&h[..h.len().min(HEAD_LEN)])
            }
        };
        let mut l1 = self.map.range::<[u8], _>((Bound::Included(l1_lo), l1_hi));
        while let Some((p, node)) = l1.next_back() {
            let head = p.as_slice();
            // Whole node above `hi` (its minimum key `head[..]` already ≥ hi)? skip.
            if !below_hi(head, &[], hi) {
                continue;
            }
            match node {
                Node::Leaf(_) => {
                    // head < hi (checked). Qualifies iff ≥ lo; else nothing below does.
                    return (head >= lo).then_some(Composite { head, tail: &[] });
                }
                Node::Branch(b) => {
                    // Seek the largest level-2 key still below hi. Only the
                    // hi-boundary branch (head == hi's 23-byte prefix) is cut by
                    // the tail; any earlier branch is wholly below hi.
                    let l2_hi: Bound<&[u8]> = match hi {
                        Bound::Excluded(h) if h.len() > HEAD_LEN && head == &h[..HEAD_LEN] => {
                            Bound::Excluded(&h[HEAD_LEN..])
                        }
                        Bound::Included(h) if h.len() > HEAD_LEN && head == &h[..HEAD_LEN] => {
                            Bound::Included(&h[HEAD_LEN..])
                        }
                        _ => Bound::Unbounded,
                    };
                    if let Some((l2, _)) =
                        b.range::<[u8], _>((Bound::Unbounded, l2_hi)).next_back()
                    {
                        return (cmp_composite(head, l2, lo) != Ordering::Less)
                            .then_some(Composite { head, tail: l2.as_slice() });
                    }
                    // No level-2 key below hi here; fall through to the previous node.
                }
            }
        }
        None
    }
}

/// Forward flattening iterator over `[lo, hi)`: walks level-1 nodes, expanding
/// each branch's level-2 map in order. Stops as soon as a key reaches `hi`.
pub(crate) struct RangeFwd<'a, 'b, V> {
    l1: btree_map::Range<'a, Prefix23, Node<V>>,
    branch: Option<(&'a [u8], btree_map::Range<'a, Level2Key, V>)>,
    lo: &'b [u8],
    hi: Bound<&'b [u8]>,
    done: bool,
}

impl<'a, 'b, V> Iterator for RangeFwd<'a, 'b, V> {
    type Item = (Composite<'a>, &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        loop {
            if let Some((head, bi)) = self.branch.as_mut() {
                let head = *head;
                match bi.next() {
                    Some((l2, v)) => {
                        if cmp_composite(head, l2, self.lo) == Ordering::Less {
                            continue; // below lo (only in the first, boundary branch)
                        }
                        if !below_hi(head, l2, self.hi) {
                            self.done = true;
                            return None;
                        }
                        return Some((
                            Composite {
                                head,
                                tail: l2.as_slice(),
                            },
                            v,
                        ));
                    }
                    None => self.branch = None,
                }
            }
            match self.l1.next()? {
                (p, Node::Leaf(v)) => {
                    let head = p.as_slice();
                    if head < self.lo {
                        continue;
                    }
                    if !below_hi(head, &[], self.hi) {
                        self.done = true;
                        return None;
                    }
                    return Some((Composite { head, tail: &[] }, v));
                }
                (p, Node::Branch(b)) => {
                    let head = p.as_slice();
                    // Whole branch already ≥ hi? then all following keys are too.
                    if !below_hi(head, &[], self.hi) {
                        self.done = true;
                        return None;
                    }
                    // Seek level-2 to the lo tail only for the boundary branch.
                    let l2_lo: Bound<&[u8]> = if self.lo.len() > HEAD_LEN && head == &self.lo[..HEAD_LEN]
                    {
                        Bound::Included(&self.lo[HEAD_LEN..])
                    } else {
                        Bound::Unbounded
                    };
                    self.branch =
                        Some((head, b.range::<[u8], _>((l2_lo, Bound::Unbounded))));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `bulk_extend_sorted` (dense build) must produce a store byte-identical to
    /// repeated `insert`, for every batch size `W` — including tiny W that splits
    /// big-contract groups across many batches (exercising the `append` merge).
    #[test]
    fn bulk_matches_insert() {
        let mut keys: Vec<Vec<u8>> = Vec::new();
        for i in 0..40u8 {
            keys.push(vec![i]); // 1B leaves
        }
        for i in 0..30u8 {
            keys.push(vec![i; 22]); // 22B leaves
        }
        for i in 0..30u8 {
            keys.push(vec![i; 23]); // 23B branch heads (empty tail)
        }
        let big = vec![200u8; 23];
        for i in 0..6000u32 {
            let mut k = big.clone();
            k.extend(i.to_be_bytes()); // one big-contract group (shared 23B prefix)
            keys.push(k);
        }
        for i in 0..1500u32 {
            let mut k = vec![100u8; 20];
            k.extend(i.to_be_bytes()); // 24B keys → mix of small groups / singletons
            keys.push(k);
        }
        keys.sort();
        keys.dedup();
        let kv: Vec<(Vec<u8>, u32)> =
            keys.iter().enumerate().map(|(i, k)| (k.clone(), i as u32)).collect();

        let mut via_insert: Entries<u32> = Entries::new();
        for (k, v) in &kv {
            via_insert.insert(k, *v);
        }
        let want: Vec<(Vec<u8>, u32)> =
            via_insert.iter().map(|(c, v)| ([c.head, c.tail].concat(), *v)).collect();

        for w in [1usize, 3, 1023, 6001, kv.len()] {
            let mut via_bulk: Entries<u32> = Entries::new();
            for chunk in kv.chunks(w) {
                via_bulk.bulk_extend_sorted(chunk.to_vec());
            }
            assert_eq!(via_bulk.len(), via_insert.len(), "len mismatch at W={w}");
            let got: Vec<(Vec<u8>, u32)> =
                via_bulk.iter().map(|(c, v)| ([c.head, c.tail].concat(), *v)).collect();
            assert_eq!(got, want, "iteration mismatch at W={w}");
        }
    }

    /// Random keys spanning the leaf/branch boundary, compared against a flat
    /// `BTreeMap` oracle for get / iterate / range endpoints.
    #[test]
    fn matches_flat_btreemap_oracle() {
        let mut seed: u64 = 0x1234_5678_9abc_def0;
        let mut next = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            (seed >> 33) as u32
        };

        let mut oracle: BTreeMap<Vec<u8>, u32> = BTreeMap::new();
        let mut ent: Entries<u32> = Entries::new();

        // Insert/overwrite/remove a mix of short (leaf) and long (branch) keys,
        // with shared prefixes to populate branches.
        let shared: Vec<u8> = (0..20).map(|_| (next() % 4) as u8).collect();
        for _ in 0..6000 {
            // Half share the 20-byte prefix (→ branches), half are arbitrary.
            let key: Vec<u8> = if next() % 2 == 0 {
                let mut k = shared.clone();
                k.extend((0..(next() % 40)).map(|_| (next() % 6) as u8));
                k
            } else {
                (0..(next() % 30)).map(|_| (next() % 6) as u8).collect()
            };
            match next() % 5 {
                0 if oracle.contains_key(&key) => {
                    oracle.remove(&key);
                    ent.remove(&key);
                }
                _ => {
                    let v = next();
                    oracle.insert(key.clone(), v);
                    ent.insert(&key, v);
                }
            }
            assert_eq!(ent.len(), oracle.len(), "len after key {key:?}");
        }

        // get
        for (k, v) in &oracle {
            assert_eq!(ent.get(k), Some(v), "get {k:?}");
        }
        assert_eq!(ent.get(&[0xff, 0xff, 0xff]), None);

        // full iteration order + values
        let got: Vec<(Vec<u8>, u32)> = ent
            .iter()
            .map(|(c, v)| ([c.head, c.tail].concat(), *v))
            .collect();
        let want: Vec<(Vec<u8>, u32)> = oracle.iter().map(|(k, v)| (k.clone(), *v)).collect();
        assert_eq!(got, want, "iteration");

        // range endpoints for many byte-prefix bounds
        let sample_keys: Vec<Vec<u8>> = oracle.keys().cloned().collect();
        for probe in sample_keys.iter().step_by(37).chain(std::iter::once(&shared)) {
            for plen in 0..=probe.len() {
                let lo = &probe[..plen];
                // exclusive upper = prefix successor
                let mut upper = lo.to_vec();
                let hi = loop {
                    match upper.last().copied() {
                        None => break Bound::Unbounded,
                        Some(0xff) => {
                            upper.pop();
                        }
                        Some(b) => {
                            *upper.last_mut().unwrap() = b + 1;
                            break Bound::Excluded(upper.clone());
                        }
                    }
                };
                let hi_ref = match &hi {
                    Bound::Excluded(v) => Bound::Excluded(v.as_slice()),
                    Bound::Included(v) => Bound::Included(v.as_slice()),
                    Bound::Unbounded => Bound::Unbounded,
                };
                let want: Vec<&Vec<u8>> = oracle.keys().filter(|k| k.starts_with(lo)).collect();
                let first = ent.range_first(lo, hi_ref);
                let last = ent.range_last(lo, hi_ref);
                match want.first() {
                    None => {
                        assert!(first.is_none(), "range_first should be empty for {lo:?}");
                        assert!(last.is_none(), "range_last should be empty for {lo:?}");
                    }
                    Some(w0) => {
                        let f = first.expect("range_first present");
                        assert_eq!(&[f.head, f.tail].concat(), *w0, "range_first {lo:?}");
                        let l = last.expect("range_last present");
                        assert_eq!(
                            &[l.head, l.tail].concat(),
                            *want.last().unwrap(),
                            "range_last {lo:?}"
                        );
                    }
                }
            }
        }
    }
}
