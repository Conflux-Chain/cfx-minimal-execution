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
//! * A level-1 value is either a **`Leaf`** — for keys ≤22 bytes, which end
//!   inside the 23-byte prefix — or a **`Branch`**: a second `BTreeMap` keyed by
//!   the key *remainder* (`key[23..]`) as a `SmallVec<u8, 40>` (48 B, inline up
//!   to 40 B — covers the 33/34-byte tails of the dominant keys with no heap
//!   allocation). The leaf-vs-branch split is by length at runtime (a key of
//!   ≤22 B can never collide with a 23-byte branch head), so the wrong-variant
//!   arms are `unreachable`.
//!
//! This store backs the **snapshot** layer only (the delta layers use the flat
//! store), so its value type is pinned to the snapshot's tombstone-free inline
//! [`SnapshotValue`].
//!
//! Flattened (level 1 in order, each branch's level 2 in order) this yields the
//! exact same sorted `(full key, value)` sequence as a flat
//! `BTreeMap<Box<[u8]>, SnapshotValue>`, but without the per-entry prefix
//! duplication or per-key heap allocation.
//! Keys are surfaced to the trie walk as a [`NibbleCursor`] view over the internal
//! `head` + `tail` slices (see `Composite`) via [`NibbleCursor::from_composite`] —
//! never reassembled into one buffer, and the split representation never escapes
//! this module.

mod prefix23;
mod range;

use crate::nibbles::{NibbleCursor, PrefixRef};
use crate::trie::reader::{TrieReader, TrieStore};
use crate::trie::SnapshotValue;
use prefix23::{Composite, Node, Prefix23};
use range::{byte_successor, prefix_upper, PrefixRange};
use smallvec::SmallVec;
use std::collections::BTreeMap;
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

/// The level-1 key of a canonical key: its first ≤23 bytes (a ≤22B key is its
/// own whole leaf prefix; a ≥23B key contributes its first 23 bytes).
fn prefix23_of(key: &[u8]) -> Prefix23 {
    Prefix23::new(&key[..key.len().min(HEAD_LEN)])
}

/// Does the split key `head ++ tail` start with the byte slice `p`, without
/// reassembling the concatenation?
fn starts_with_composite(head: &[u8], tail: &[u8], p: &[u8]) -> bool {
    if p.len() <= head.len() {
        head.starts_with(p)
    } else {
        let (in_head, in_tail) = p.split_at(head.len());
        head == in_head && tail.len() >= in_tail.len() && &tail[..in_tail.len()] == in_tail
    }
}

/// Read byte `i` of the split key `head ++ tail` (caller ensures `i` is in range).
#[inline]
fn composite_byte(head: &[u8], tail: &[u8], i: usize) -> u8 {
    match i.checked_sub(head.len()) {
        Some(j) => tail[j],
        None => head[i],
    }
}

/// Does the full canonical key `head ++ tail` carry the nibble `prefix`?
/// **Zero-alloc**: matched directly against the packed prefix bytes + parity —
/// no byte bound materialized, no key reassembled. This is the membership
/// predicate that drives every forward prefix scan.
fn under_prefix(prefix: PrefixRef, head: &[u8], tail: &[u8]) -> bool {
    let qp = prefix.packed();
    if qp.is_empty() {
        return true; // the empty prefix carries every key
    }
    // Fully-constrained leading bytes, plus (for an odd prefix) one byte whose
    // HIGH nibble must match.
    let (full, half) = match qp.split_last() {
        Some((last, rest)) if !prefix.is_even() => (rest, Some(last >> 4)),
        _ => (qp, None),
    };
    if !starts_with_composite(head, tail, full) {
        return false;
    }
    match half {
        None => true,
        Some(h) => {
            let m = full.len();
            head.len() + tail.len() > m && composite_byte(head, tail, m) >> 4 == h
        }
    }
}

/// Inline capacity for a reverse-seek anchor: canonical keys are ≤57 bytes, so
/// level-1 heads (≤23 B) and level-2 tails (≤34 B) both fit with no heap.
type Anchor = SmallVec<u8, 48>;

/// Borrow an [`Anchor`] bound as a slice bound for `BTreeMap::range`.
fn anchor_ref(b: &Bound<Anchor>) -> Bound<&[u8]> {
    match b {
        Bound::Included(v) => Bound::Included(v.as_ref()),
        Bound::Excluded(v) => Bound::Excluded(v.as_ref()),
        Bound::Unbounded => Bound::Unbounded,
    }
}

/// The [`TwoLevelStore`] store: a level-1 `BTreeMap` of [`Prefix23`] → [`Node`].
#[derive(Clone, Default)]
pub(crate) struct TwoLevelStore {
    map: BTreeMap<Prefix23, Node>,
    len: usize,
}

// Manual Debug: a summary (sizes), never the millions of entries.
impl std::fmt::Debug for TwoLevelStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TwoLevelStore")
            .field("len", &self.len)
            .field("level1_nodes", &self.map.len())
            .finish()
    }
}

impl TwoLevelStore {
    pub(crate) fn new() -> Self {
        Self {
            map: BTreeMap::new(),
            len: 0,
        }
    }

    // Inherent `len`/`is_empty`: the trie/engine reach these through the
    // `TrieReader` impl below; the inherent forms are used only by the tests in
    // this module, so they read dead in the lib build alone.
    #[inline]
    #[allow(dead_code)]
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    #[inline]
    #[allow(dead_code)]
    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }

    // Reached only through the `TrieStore::clear` impl → `AuthenticatedTrie::clear`
    // (test-only path); reads dead in the lib build alone.
    #[allow(dead_code)]
    pub(crate) fn clear(&mut self) {
        self.map.clear();
        self.len = 0;
    }

    pub(crate) fn insert(&mut self, key: &[u8], value: SnapshotValue) {
        if key.len() <= LEAF_MAX {
            if self
                .map
                .insert(Prefix23::new(key), Node::Leaf(value))
                .is_none()
            {
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
    pub(crate) fn bulk_extend_sorted(&mut self, batch: Vec<(Vec<u8>, SnapshotValue)>) {
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
                let mut chunk: BTreeMap<Level2Key, SnapshotValue> =
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

    pub(crate) fn get(&self, key: &[u8]) -> Option<&SnapshotValue> {
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

    /// Forward iterator over all entries carrying nibble `prefix`, in sorted key
    /// order. **Zero-alloc**: seeks to the prefix's borrowed lower bound
    /// ([`PrefixKey::packed`]) and stops at the first key that no longer
    /// carries the prefix ([`under_prefix`]) — no `(lo, hi)` byte pair is ever
    /// materialized (contrast the old `key_bounds` → `range` path, which
    /// heap-allocated the bounds on every call, 16×/node in the trie walk).
    pub(crate) fn prefix_range<'a, 'b>(&'a self, prefix: PrefixRef<'b>) -> PrefixRange<'a, 'b> {
        let lo = prefix.packed();
        let l1_lo = &lo[..lo.len().min(HEAD_LEN)];
        PrefixRange {
            l1: self
                .map
                .range::<[u8], _>((Bound::Included(l1_lo), Bound::Unbounded)),
            branch: None,
            prefix: Some(prefix),
            done: false,
        }
    }

    /// Full iterator over all entries in sorted key order.
    pub(crate) fn iter(&self) -> PrefixRange<'_, 'static> {
        PrefixRange {
            l1: self
                .map
                .range::<[u8], _>((Bound::Included(&[][..]), Bound::Unbounded)),
            branch: None,
            prefix: None,
            done: false,
        }
    }

    /// First key carrying `prefix`, or `None` if none, as a [`NibbleCursor`] view.
    /// Zero-alloc.
    pub(crate) fn prefix_first(&self, prefix: PrefixRef) -> Option<NibbleCursor<'_>> {
        self.prefix_range(prefix).next().map(|(n, _)| n)
    }

    /// Whether any key carries `prefix`. The trie walk's per-child existence
    /// probe (16×/node), so it does the **least** work: one level-1 seek, and
    /// only for a tail-reaching prefix does it also probe level 2. Zero-alloc.
    pub(crate) fn prefix_exists(&self, prefix: PrefixRef) -> bool {
        let packed = prefix.packed();
        let l1_lo = &packed[..packed.len().min(HEAD_LEN)];
        let Some((p, node)) = self
            .map
            .range::<[u8], _>((Bound::Included(l1_lo), Bound::Unbounded))
            .next()
        else {
            return false;
        };
        let head = p.as_slice();
        if packed.len() > HEAD_LEN {
            // Prefix reaches into the tail: only `head == packed[..23]` (a branch)
            // can carry it, and only if it holds a tail under `packed[23..]`.
            match node {
                Node::Branch(b) if head == &packed[..HEAD_LEN] => {
                    let tail_lo = &packed[HEAD_LEN..];
                    let tail_hi = prefix_upper(tail_lo, prefix.is_even());
                    b.range::<[u8], _>((Bound::Included(tail_lo), anchor_ref(&tail_hi)))
                        .next()
                        .is_some()
                }
                _ => false,
            }
        } else {
            // Prefix lies within the head: the first node ≥ the prefix carries it
            // iff its head does — a leaf's whole key, or a (never-empty) branch's
            // shared head. If not, nothing sorted after it does either. No level-2
            // probe needed (this is the common case in the child walk).
            under_prefix(prefix, head, &[])
        }
    }

    /// Last key carrying `prefix`, or `None` if none. A **native reverse
    /// traversal** driven by the prefix: the lower bound is borrowed
    /// (`packed()`), and the two reverse-seek anchors (level-1 head ceiling,
    /// level-2 tail ceiling) are computed inline into an [`Anchor`] — no heap,
    /// no `(lo, hi)` byte pair. Bounded by the prefix's ceiling, so `next_back`
    /// starts just below it: `O(log n)` per level, never a scan from the map's
    /// global maximum.
    pub(crate) fn prefix_last(&self, prefix: PrefixRef) -> Option<NibbleCursor<'_>> {
        let packed = prefix.packed();
        let l1_lo = &packed[..packed.len().min(HEAD_LEN)];
        // A prefix reaching past the 23-byte head pins a single level-1 node
        // (`head == packed[..23]`) and cuts its tails; a shorter prefix selects a
        // run of whole level-1 nodes.
        let into_tail = packed.len() > HEAD_LEN;
        let l1_hi = if into_tail {
            byte_successor(&packed[..HEAD_LEN])
        } else {
            prefix_upper(packed, prefix.is_even())
        };
        // The seek range holds only prefix-carrying level-1 nodes, so its top —
        // one `next_back` — is the answer: regime-2 bounds it to a single node,
        // and regime-1's highest node always yields the max key. No scan.
        let (p, node) = self
            .map
            .range::<[u8], _>((Bound::Included(l1_lo), anchor_ref(&l1_hi)))
            .next_back()?;
        let head = p.as_slice();
        let last = match node {
            Node::Leaf(_) => {
                under_prefix(prefix, head, &[]).then_some(Composite { head, tail: &[] })
            }
            Node::Branch(b) if into_tail => {
                // Boundary branch: its largest tail under `packed[23..]`.
                let tail_lo = &packed[HEAD_LEN..];
                let tail_hi = prefix_upper(tail_lo, prefix.is_even());
                b.range::<[u8], _>((Bound::Included(tail_lo), anchor_ref(&tail_hi)))
                    .next_back()
                    .map(|(l2, _)| Composite {
                        head,
                        tail: l2.as_slice(),
                    })
            }
            Node::Branch(b) => {
                // Whole branch is under the prefix → its largest tail wins.
                b.iter().next_back().map(|(l2, _)| Composite {
                    head,
                    tail: l2.as_slice(),
                })
            }
        };
        last.map(|c| c.nibbles())
    }
}

/// The two-level store as seen by the root/memo traversal: each method forwards
/// to the inherent method it abstracts, flattening the value to merkle bytes at
/// the two value-touching points (`get_merkle`, `iter_merkle_prefix`) so the engine
/// never names the value type. A [`SnapshotValue`]'s merkle bytes are simply its
/// bytes (it is always live — no tombstone variant).
impl TrieReader for TwoLevelStore {
    #[inline]
    fn len(&self) -> usize {
        self.len
    }

    #[inline]
    fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn get_merkle(&self, key: PrefixRef) -> Option<&[u8]> {
        key.as_canonical_key()
            .and_then(|ck| self.get(ck))
            .map(|v| v.as_slice())
    }

    fn prefix_exists(&self, prefix: PrefixRef) -> bool {
        TwoLevelStore::prefix_exists(self, prefix)
    }

    fn prefix_first(&self, prefix: PrefixRef) -> Option<NibbleCursor<'_>> {
        TwoLevelStore::prefix_first(self, prefix)
    }

    fn prefix_last(&self, prefix: PrefixRef) -> Option<NibbleCursor<'_>> {
        TwoLevelStore::prefix_last(self, prefix)
    }

    fn iter_merkle_prefix<'p>(
        &self,
        prefix: PrefixRef<'p>,
    ) -> impl Iterator<Item = (NibbleCursor<'_>, &[u8])> {
        self.prefix_range(prefix).map(|(n, v)| (n, v.as_slice()))
    }
}

/// The two-level store's write side: each method forwards to the inherent method
/// it abstracts (inherent methods win name resolution, so `self.insert(..)` calls
/// the concrete one, not this trait method).
impl TrieStore for TwoLevelStore {
    type Value = SnapshotValue;

    #[inline]
    fn insert(&mut self, key: &[u8], value: SnapshotValue) {
        self.insert(key, value);
    }

    #[inline]
    fn remove(&mut self, key: &[u8]) {
        self.remove(key);
    }

    #[inline]
    fn clear(&mut self) {
        self.clear();
    }

    #[inline]
    fn bulk_extend_sorted(&mut self, batch: Vec<(Vec<u8>, SnapshotValue)>) {
        self.bulk_extend_sorted(batch);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trie::snapshot_value;

    /// A small distinct test value from a `u32` tag.
    fn sv(x: u32) -> SnapshotValue {
        snapshot_value(&x.to_le_bytes())
    }

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
        let kv: Vec<(Vec<u8>, SnapshotValue)> = keys
            .iter()
            .enumerate()
            .map(|(i, k)| (k.clone(), sv(i as u32)))
            .collect();

        let mut via_insert = TwoLevelStore::new();
        for (k, v) in &kv {
            via_insert.insert(k, v.clone());
        }
        let want: Vec<(Vec<u8>, SnapshotValue)> = via_insert
            .iter()
            .map(|(n, v)| (n.to_canonical_bytes(), v.clone()))
            .collect();

        for w in [1usize, 3, 1023, 6001, kv.len()] {
            let mut via_bulk = TwoLevelStore::new();
            for chunk in kv.chunks(w) {
                via_bulk.bulk_extend_sorted(chunk.to_vec());
            }
            assert_eq!(via_bulk.len(), via_insert.len(), "len mismatch at W={w}");
            let got: Vec<(Vec<u8>, SnapshotValue)> = via_bulk
                .iter()
                .map(|(n, v)| (n.to_canonical_bytes(), v.clone()))
                .collect();
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

        let mut oracle: BTreeMap<Vec<u8>, SnapshotValue> = BTreeMap::new();
        let mut ent = TwoLevelStore::new();

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
                    let v = sv(next());
                    oracle.insert(key.clone(), v.clone());
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
        let got: Vec<(Vec<u8>, SnapshotValue)> = ent
            .iter()
            .map(|(n, v)| (n.to_canonical_bytes(), v.clone()))
            .collect();
        let want: Vec<(Vec<u8>, SnapshotValue)> =
            oracle.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        assert_eq!(got, want, "iteration");

        // prefix_first / prefix_last / prefix_exists / prefix_range for EVERY
        // nibble prefix (even AND odd) of many sample keys. The odd lengths
        // exercise the packed half-nibble bound (incl. the nibble-15 carry).
        let expand = |k: &[u8]| -> Vec<u8> {
            let mut v = Vec::with_capacity(k.len() * 2);
            for b in k {
                v.push(b >> 4);
                v.push(b & 0x0f);
            }
            v
        };
        let sample_keys: Vec<Vec<u8>> = oracle.keys().cloned().collect();
        for probe in sample_keys
            .iter()
            .step_by(37)
            .chain(std::iter::once(&shared))
        {
            let pnib = expand(probe);
            for nlen in 0..=pnib.len() {
                let buf =
                    crate::nibbles::pack_nibbles(NibbleCursor::from_bytes(probe).sub(0, nlen));
                let prefix = PrefixRef::new(&buf);
                // Oracle: keys whose nibble expansion carries `pnib[..nlen]`,
                // already in byte-sorted order (BTreeMap iteration).
                let want: Vec<&Vec<u8>> = oracle
                    .keys()
                    .filter(|k| {
                        let e = expand(k);
                        e.len() >= nlen && e[..nlen] == pnib[..nlen]
                    })
                    .collect();

                assert_eq!(
                    ent.prefix_exists(prefix),
                    !want.is_empty(),
                    "prefix_exists n={nlen} probe={probe:?}"
                );

                let got: Vec<Vec<u8>> = ent
                    .prefix_range(prefix)
                    .map(|(n, _)| n.to_canonical_bytes())
                    .collect();
                let want_v: Vec<Vec<u8>> = want.iter().map(|k| (*k).clone()).collect();
                assert_eq!(got, want_v, "prefix_range n={nlen} probe={probe:?}");

                match (want.first(), want.last()) {
                    (None, _) => {
                        assert!(ent.prefix_first(prefix).is_none(), "first empty n={nlen}");
                        assert!(ent.prefix_last(prefix).is_none(), "last empty n={nlen}");
                    }
                    (Some(w0), Some(wl)) => {
                        let f = ent.prefix_first(prefix).expect("first present");
                        assert_eq!(&f.to_canonical_bytes(), *w0, "prefix_first n={nlen}");
                        let l = ent.prefix_last(prefix).expect("last present");
                        assert_eq!(&l.to_canonical_bytes(), *wl, "prefix_last n={nlen}");
                    }
                    _ => unreachable!(),
                }
            }
        }
    }
}
