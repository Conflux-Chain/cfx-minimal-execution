//! Flat entry store for the incremental trie.
//!
//! The simplest possible backing: a single `BTreeMap<Box<[u8]>, V>` keyed on the
//! canonical byte key. It carries none of the two-level store's prefix-factoring
//! machinery (no `Prefix23` / level-2 split) — every key stores its full bytes —
//! but it yields the **exact same sorted `(key, V)` sequence**, so the root/memo
//! walk computes an identical root over it. That is the whole point of the
//! [`TrieReader`] abstraction: the engine sees only the ordered merkle-byte stream
//! and never the store's internal shape, so `flat` and `two_level` are
//! interchangeable behind it (proven by the oracle test below and the
//! `incremental_trie` differential fuzz, which drives both stores through the same
//! engine and asserts equal roots).
//!
//! This store is the delta layer's backing (a delta is small and hot, so the
//! two-level prefix factoring buys nothing there): [`crate::state::layers::delta`]
//! and [`crate::state::layers::intermediate`] are both [`BTreeStore`]s, collapsing
//! the historical `State.delta` `BTreeMap` + `delta_inc` two-level duplicate into
//! this one store. The value type is pinned to [`MptValue`] — the delta is the
//! layer where tombstones are meaningful.
//!
//! Keys are surfaced to the walk as a [`NibbleCursor`] view over the stored byte key
//! ([`NibbleCursor::from_bytes`]) — semantically identical to the two-level store's
//! `head ++ tail` composite view, since both express the same canonical key.

use crate::nibbles::{NibbleCursor, PrefixRef};
use crate::trie::reader::{TrieReader, TrieStore};
use crate::trie::MptValue;
use std::collections::BTreeMap;
use std::ops::Bound;

/// Smallest byte string strictly greater than every string having `p` as a
/// prefix: increment the last non-`0xff` byte, dropping trailing `0xff`. An
/// all-`0xff` (or empty) `p` has no successor → the range is open-ended.
/// Byte-for-byte identical to the two-level store's helper — the two stores must
/// resolve prefix bounds the same way to iterate the same key set.
fn byte_successor(p: &[u8]) -> Bound<Vec<u8>> {
    let mut v = p.to_vec();
    while let Some(&last) = v.last() {
        if last == 0xff {
            v.pop();
        } else {
            let n = v.len();
            v[n - 1] = last + 1;
            return Bound::Excluded(v);
        }
    }
    Bound::Unbounded
}

/// Exclusive upper byte-bound of the packed prefix `packed` (parity `even`): the
/// first canonical key NOT under it. Parity-aware — an odd prefix's trailing
/// half-nibble `h` ends the range at high-nibble `h + 1`, carrying out of the
/// byte when `h == 15`. Mirrors the two-level store's `prefix_upper`.
fn prefix_upper(packed: &[u8], even: bool) -> Bound<Vec<u8>> {
    if even {
        return byte_successor(packed);
    }
    let (last, rest) = packed.split_last().expect("odd prefix has ≥1 byte");
    let h = last >> 4;
    if h < 15 {
        let mut u = rest.to_vec();
        u.push((h + 1) << 4);
        Bound::Excluded(u)
    } else {
        byte_successor(rest)
    }
}

/// Borrow an owned bound as a slice bound for `BTreeMap::range`.
fn anchor_ref(b: &Bound<Vec<u8>>) -> Bound<&[u8]> {
    match b {
        Bound::Included(v) => Bound::Included(v.as_slice()),
        Bound::Excluded(v) => Bound::Excluded(v.as_slice()),
        Bound::Unbounded => Bound::Unbounded,
    }
}

/// A flat `BTreeMap` entry store: canonical byte key → [`MptValue`].
#[derive(Clone, Default)]
pub(crate) struct BTreeStore {
    map: BTreeMap<Box<[u8]>, MptValue>,
}

// Manual Debug: a summary (size), never the entries (mirrors `TwoLevelStore`).
impl std::fmt::Debug for BTreeStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BTreeStore")
            .field("len", &self.map.len())
            .finish()
    }
}

impl BTreeStore {
    pub(crate) fn new() -> Self {
        Self {
            map: BTreeMap::new(),
        }
    }

    /// The `(inclusive lower, exclusive upper)` byte range holding exactly the
    /// keys carrying nibble `prefix`. Keys are byte-aligned, so byte order equals
    /// nibble order and this range is exactly the prefix's key set — no per-key
    /// membership test needed.
    fn prefix_bounds(prefix: PrefixRef<'_>) -> (Bound<&[u8]>, Bound<Vec<u8>>) {
        let lo = prefix.packed();
        let hi = prefix_upper(lo, prefix.is_even());
        (Bound::Included(lo), hi)
    }

    // --- Value-typed access used by the delta / intermediate state layers. These
    // operate on the raw canonical byte key (for the delta layers that key is the
    // delta-mpt-padded key) and never touch the nibble-routing machinery above. ---

    /// Number of stored entries.
    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.map.len()
    }

    /// Point lookup by exact byte key.
    #[inline]
    pub(crate) fn get(&self, key: &[u8]) -> Option<&MptValue> {
        self.map.get(key)
    }

    /// The inner map, for direct (bincode) serialization of the layer — a
    /// `BTreeMap<Box<[u8]>, MptValue>` serializes byte-for-byte identically to the
    /// historical `BTreeMap<Vec<u8>, MptValue>` (same sort order, same `[u8]` seq
    /// encoding of each key), so the checkpoint bytes are unchanged.
    #[inline]
    pub(crate) fn raw_entries(&self) -> &BTreeMap<Box<[u8]>, MptValue> {
        &self.map
    }

    /// Consume the store, yielding its inner map (used to MOVE a finished delta's
    /// store into the intermediate layer at rotation, with no re-materialization).
    #[inline]
    pub(crate) fn into_entries(self) -> BTreeMap<Box<[u8]>, MptValue> {
        self.map
    }

    /// Ascending byte-**prefix** scan: every entry whose full byte key starts with
    /// `prefix`. This is the state-level (delta-mpt-padded) prefix scan — a plain
    /// byte `starts_with`, byte-for-byte identical to the historical
    /// `BTreeMap::range(prefix..).take_while(|k| k.starts_with(prefix))`; it is NOT
    /// the nibble-prefix routing the trie walk uses ([`TrieReader::prefix_first`]).
    pub(crate) fn scan_byte_prefix<'a>(
        &'a self,
        prefix: &'a [u8],
    ) -> impl Iterator<Item = (&'a [u8], &'a MptValue)> {
        self.map
            .range::<[u8], _>((Bound::Included(prefix), Bound::Unbounded))
            .take_while(move |(k, _)| k.starts_with(prefix))
            .map(|(k, v)| (&k[..], v))
    }

    /// Materialize a `BTreeMap<Vec<u8>, MptValue>` copy of the store (for the
    /// checkpoint `PersistedState` image and the `verify-incremental` oracle
    /// cross-check).
    pub(crate) fn to_value_map(&self) -> BTreeMap<Vec<u8>, MptValue> {
        self.map
            .iter()
            .map(|(k, v)| (k.to_vec(), v.clone()))
            .collect()
    }
}

/// The flat store as seen by the root/memo traversal. Every method has the same
/// semantics as the two-level store's [`TrieReader`] impl — same nibble views,
/// same merkle bytes, same prefix/singleton behaviour — which is what lets the
/// engine produce identical roots regardless of backend. The value is flattened
/// to merkle bytes at the two value-touching points (`get_merkle`,
/// `iter_merkle_prefix`), so the engine never names the value type.
impl TrieReader for BTreeStore {
    #[inline]
    fn len(&self) -> usize {
        self.map.len()
    }

    #[inline]
    fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    fn get_merkle(&self, key: PrefixRef) -> Option<&[u8]> {
        key.as_canonical_key()
            .and_then(|ck| self.map.get(ck))
            .map(|v| v.merkle_value())
    }

    fn prefix_exists(&self, prefix: PrefixRef) -> bool {
        let (lo, hi) = Self::prefix_bounds(prefix);
        self.map
            .range::<[u8], _>((lo, anchor_ref(&hi)))
            .next()
            .is_some()
    }

    fn prefix_first(&self, prefix: PrefixRef) -> Option<NibbleCursor<'_>> {
        let (lo, hi) = Self::prefix_bounds(prefix);
        self.map
            .range::<[u8], _>((lo, anchor_ref(&hi)))
            .next()
            .map(|(k, _)| NibbleCursor::from_bytes(&k[..]))
    }

    fn prefix_last(&self, prefix: PrefixRef) -> Option<NibbleCursor<'_>> {
        let (lo, hi) = Self::prefix_bounds(prefix);
        self.map
            .range::<[u8], _>((lo, anchor_ref(&hi)))
            .next_back()
            .map(|(k, _)| NibbleCursor::from_bytes(&k[..]))
    }

    fn iter_merkle_prefix<'p>(
        &self,
        prefix: PrefixRef<'p>,
    ) -> impl Iterator<Item = (NibbleCursor<'_>, &[u8])> {
        // Owned bounds (cold-path only, a handful of calls per rebuild), so the
        // returned iterator borrows nothing but the map.
        let lo = Bound::Included(Box::<[u8]>::from(prefix.packed()));
        let hi = match prefix_upper(prefix.packed(), prefix.is_even()) {
            Bound::Included(v) => Bound::Included(v.into_boxed_slice()),
            Bound::Excluded(v) => Bound::Excluded(v.into_boxed_slice()),
            Bound::Unbounded => Bound::Unbounded,
        };
        self.map
            .range::<Box<[u8]>, _>((lo, hi))
            .map(|(k, v)| (NibbleCursor::from_bytes(&k[..]), v.merkle_value()))
    }
}

/// The flat store's write side — a thin wrapper over the inner `BTreeMap`.
impl TrieStore for BTreeStore {
    type Value = MptValue;

    #[inline]
    fn insert(&mut self, key: &[u8], value: MptValue) {
        self.map.insert(Box::from(key), value);
    }

    #[inline]
    fn remove(&mut self, key: &[u8]) {
        self.map.remove(key);
    }

    #[inline]
    fn clear(&mut self) {
        self.map.clear();
    }

    #[inline]
    fn bulk_extend_sorted(&mut self, batch: Vec<(Vec<u8>, MptValue)>) {
        // The batch is ascending and disjoint from what's present, so a plain
        // extend inserts each key once (no reordering work for a `BTreeMap`).
        self.map
            .extend(batch.into_iter().map(|(k, v)| (k.into_boxed_slice(), v)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trie::{trie_root, AuthenticatedTrie, MptValue};
    use std::collections::BTreeMap;

    fn val(b: u8) -> MptValue {
        if b == 0 {
            MptValue::Tombstone
        } else {
            MptValue::Some(Box::from([b]))
        }
    }

    /// Drive an `AuthenticatedTrie<BTreeStore>` through a random insert/remove
    /// op sequence and assert its incremental `root()` equals the from-scratch
    /// stateless `trie_root` oracle — mirroring the two-level store's oracle
    /// tests. Proves the flat backend computes correct roots through the engine.
    #[test]
    fn flat_matches_oracle_under_random_ops() {
        let mut seed: u64 = 0x9e3779b97f4a7c15;
        let mut next = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            (seed >> 33) as u32
        };

        let mut map: BTreeMap<Vec<u8>, MptValue> = BTreeMap::new();
        let mut trie = AuthenticatedTrie::<BTreeStore>::default();

        for op_i in 0..4000 {
            // Keys share prefixes to exercise path compression split/merge.
            let klen = 1 + (next() % 4) as usize;
            let key: Vec<u8> = (0..klen).map(|_| (next() % 6) as u8).collect();
            let v = (next() % 4) as u8;

            let action = if v == 0 && map.contains_key(&key) {
                map.remove(&key);
                trie.remove(&key);
                "remove"
            } else {
                map.insert(key.clone(), val(v));
                trie.insert(&key, val(v));
                "insert"
            };

            // Compare on ~1/3 of ops (batched multi-dirty commits in between).
            if next() % 3 == 0 {
                assert_eq!(
                    trie.root(),
                    trie_root(&map),
                    "diverged at op {op_i}: {action} key={key:?} size {}",
                    map.len()
                );
            }
        }
        assert_eq!(trie.root(), trie_root(&map));
    }

    /// Same, but compare after *every* op so a single-op update bug cannot hide
    /// behind a later full recompute.
    #[test]
    fn flat_matches_oracle_every_op() {
        let mut seed: u64 = 0x2545f4914f6cdd1d;
        let mut next = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            (seed >> 33) as u32
        };
        let mut map: BTreeMap<Vec<u8>, MptValue> = BTreeMap::new();
        let mut trie = AuthenticatedTrie::<BTreeStore>::default();
        for op_i in 0..4000 {
            let klen = 1 + (next() % 5) as usize;
            let key: Vec<u8> = (0..klen).map(|_| (next() % 6) as u8).collect();
            let v = (next() % 4) as u8;
            if v == 0 && map.contains_key(&key) {
                map.remove(&key);
                trie.remove(&key);
            } else {
                map.insert(key.clone(), val(v));
                trie.insert(&key, val(v));
            }
            assert_eq!(
                trie.root(),
                trie_root(&map),
                "diverged at op {op_i} key={key:?}"
            );
        }
    }

    /// Wide byte-aligned keys (like real 20/57-byte mpt keys) with long shared
    /// prefixes and odd routing depths — exercises the packed prefix-bound
    /// translation (incl. the nibble-15 carry) against the oracle.
    #[test]
    fn flat_wide_byte_keys_match_oracle() {
        let mut seed: u64 = 0xd1b54a32d192ed03;
        let mut next = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            (seed >> 24) as u8
        };
        let mut map: BTreeMap<Vec<u8>, MptValue> = BTreeMap::new();
        let mut trie = AuthenticatedTrie::<BTreeStore>::default();
        let account: Vec<u8> = (0..20).map(|_| next()).collect();
        for _ in 0..3000 {
            let mut key = account.clone();
            key.extend((0..37).map(|_| next()));
            let v = MptValue::Some(Box::from([next(), next()]));
            trie.insert(&key, v.clone());
            map.insert(key, v);
        }
        for _ in 0..500 {
            let key: Vec<u8> = (0..(1 + (next() % 30) as usize)).map(|_| next()).collect();
            let v = MptValue::Some(Box::from([next()]));
            trie.insert(&key, v.clone());
            map.insert(key, v);
        }
        assert_eq!(trie.root(), trie_root(&map));
    }

    /// Cold parallel-rebuild path (>= threshold entries, empty cache, >1 child at
    /// the root — via `from_store`) with tombstones interleaved. Proves a terminal
    /// tombstone still hashes as `Some(&[])` (the `b'v'` marker in
    /// `compute_node_merkle`) — byte-identical to the stateless oracle. The
    /// Some/None distinction is carried by key-presence (`find` on nibble
    /// length), which the flattening to merkle bytes does not touch;
    /// `merkle_value()` collapses tombstone and empty `Some` to the same `&[]`,
    /// exactly as the oracle does.
    #[test]
    fn parallel_rebuild_with_tombstones_matches_oracle() {
        use crate::trie::root::PARALLEL_REBUILD_THRESHOLD;
        let mut map: BTreeMap<Vec<u8>, MptValue> = BTreeMap::new();
        let mut store = BTreeStore::new();
        for i in 0u32..8000 {
            let key = vec![(i & 0xff) as u8, (i >> 8) as u8, ((i >> 3) & 0xff) as u8];
            // Every 5th entry is a tombstone (some become terminal-value nodes);
            // one real entry is deliberately an empty value to exercise the
            // tombstone-vs-empty-`Some` collapse.
            let v = if i % 5 == 0 {
                MptValue::Tombstone
            } else if i % 5 == 1 {
                MptValue::Some(Box::from([]))
            } else {
                MptValue::Some(Box::from([(i as u8) | 1]))
            };
            store.insert(&key, v.clone());
            map.insert(key, v);
        }
        assert!(map.len() >= PARALLEL_REBUILD_THRESHOLD);
        let mut trie = AuthenticatedTrie::from_store(store);
        assert_eq!(trie.root(), trie_root(&map));
    }
}
