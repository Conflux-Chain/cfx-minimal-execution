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
//! and [`crate::state::layers::intermediate`] are both `FlatStore<MptValue>`,
//! collapsing the historical `State.delta` `BTreeMap` + `delta_inc` two-level
//! duplicate into this one store.
//!
//! Keys are surfaced to the walk as a [`Nibbles`] view over the stored byte key
//! ([`Nibbles::from_bytes`]) — semantically identical to the two-level store's
//! `head ++ tail` composite view, since both express the same canonical key.

use crate::nibbles::{NibbleRef, Nibbles};
use crate::trie::reader::{TrieReader, TrieStore};
use crate::trie::TrieValue;
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

/// A flat `BTreeMap` entry store: canonical byte key → value.
pub(crate) struct FlatStore<V> {
    map: BTreeMap<Box<[u8]>, V>,
}

impl<V> Default for FlatStore<V> {
    fn default() -> Self {
        Self::new()
    }
}

// Manual Clone/Debug so callers don't need `V: Debug` (mirrors `Entries`).
impl<V: Clone> Clone for FlatStore<V> {
    fn clone(&self) -> Self {
        Self {
            map: self.map.clone(),
        }
    }
}

impl<V> std::fmt::Debug for FlatStore<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FlatStore")
            .field("len", &self.map.len())
            .finish()
    }
}

impl<V> FlatStore<V> {
    pub(crate) fn new() -> Self {
        Self {
            map: BTreeMap::new(),
        }
    }

    /// The `(inclusive lower, exclusive upper)` byte range holding exactly the
    /// keys carrying nibble `prefix`. Keys are byte-aligned, so byte order equals
    /// nibble order and this range is exactly the prefix's key set — no per-key
    /// membership test needed.
    fn prefix_bounds(prefix: NibbleRef<'_>) -> (Bound<&[u8]>, Bound<Vec<u8>>) {
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
    pub(crate) fn get(&self, key: &[u8]) -> Option<&V> {
        self.map.get(key)
    }

    /// The inner map, for direct (bincode) serialization of the layer — a
    /// `BTreeMap<Box<[u8]>, V>` serializes byte-for-byte identically to the
    /// historical `BTreeMap<Vec<u8>, V>` (same sort order, same `[u8]` seq
    /// encoding of each key), so the checkpoint bytes are unchanged.
    #[inline]
    pub(crate) fn raw_entries(&self) -> &BTreeMap<Box<[u8]>, V> {
        &self.map
    }

    /// Consume the store, yielding its inner map (used to MOVE a finished delta's
    /// store into the intermediate layer at rotation, with no re-materialization).
    #[inline]
    pub(crate) fn into_entries(self) -> BTreeMap<Box<[u8]>, V> {
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
    ) -> impl Iterator<Item = (&'a [u8], &'a V)> {
        self.map
            .range::<[u8], _>((Bound::Included(prefix), Bound::Unbounded))
            .take_while(move |(k, _)| k.starts_with(prefix))
            .map(|(k, v)| (&k[..], v))
    }

    /// Materialize a `BTreeMap<Vec<u8>, V>` copy of the store (for the checkpoint
    /// `PersistedState` image and the `verify-incremental` oracle cross-check).
    pub(crate) fn to_value_map(&self) -> BTreeMap<Vec<u8>, V>
    where
        V: Clone,
    {
        self.map.iter().map(|(k, v)| (k.to_vec(), v.clone())).collect()
    }
}

/// The flat store as seen by the root/memo traversal. Every method has the same
/// semantics as the two-level store's [`TrieReader`] impl — same nibble views,
/// same merkle bytes, same prefix/singleton behaviour — which is what lets the
/// engine produce identical roots regardless of backend. `V` is flattened to
/// merkle bytes at the two `V`-touching points (`get_merkle`, `iter_merkle`), so
/// the engine never names the value type.
impl<V: TrieValue> TrieReader for FlatStore<V> {
    #[inline]
    fn len(&self) -> usize {
        self.map.len()
    }

    #[inline]
    fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    fn get_merkle(&self, key: NibbleRef) -> Option<&[u8]> {
        key.as_canonical_key()
            .and_then(|ck| self.map.get(ck))
            .map(|v| v.merkle_value())
    }

    fn prefix_exists(&self, prefix: NibbleRef) -> bool {
        let (lo, hi) = Self::prefix_bounds(prefix);
        self.map
            .range::<[u8], _>((lo, anchor_ref(&hi)))
            .next()
            .is_some()
    }

    fn prefix_first(&self, prefix: NibbleRef) -> Option<Nibbles<'_>> {
        let (lo, hi) = Self::prefix_bounds(prefix);
        self.map
            .range::<[u8], _>((lo, anchor_ref(&hi)))
            .next()
            .map(|(k, _)| Nibbles::from_bytes(&k[..]))
    }

    fn prefix_last(&self, prefix: NibbleRef) -> Option<Nibbles<'_>> {
        let (lo, hi) = Self::prefix_bounds(prefix);
        self.map
            .range::<[u8], _>((lo, anchor_ref(&hi)))
            .next_back()
            .map(|(k, _)| Nibbles::from_bytes(&k[..]))
    }

    fn iter_merkle(&self) -> impl Iterator<Item = (Nibbles<'_>, &[u8])> {
        self.map
            .iter()
            .map(|(k, v)| (Nibbles::from_bytes(&k[..]), v.merkle_value()))
    }
}

/// The flat store's write side — a thin wrapper over the inner `BTreeMap`.
impl<V: TrieValue> TrieStore for FlatStore<V> {
    type Value = V;

    #[inline]
    fn insert(&mut self, key: &[u8], value: V) {
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
    fn bulk_extend_sorted(&mut self, batch: Vec<(Vec<u8>, V)>) {
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

    /// Drive an `AuthenticatedTrie<FlatStore<..>>` through a random insert/remove
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
        let mut trie = AuthenticatedTrie::<FlatStore<MptValue>>::default();

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
        let mut trie = AuthenticatedTrie::<FlatStore<MptValue>>::default();
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
            assert_eq!(trie.root(), trie_root(&map), "diverged at op {op_i} key={key:?}");
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
        let mut trie = AuthenticatedTrie::<FlatStore<MptValue>>::default();
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
}
