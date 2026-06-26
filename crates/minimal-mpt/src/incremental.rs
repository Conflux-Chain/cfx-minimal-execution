//! Incremental delta-trie root.
//!
//! The stateless [`crate::trie::trie_root`] rebuilds and re-hashes the entire
//! trie on every call. Inside a snapshot period the delta only grows, so doing
//! that every epoch is `O(N)` per commit and `O(N²)` per period — profiling
//! showed it dominating ~86% of replay CPU.
//!
//! [`IncrementalTrie`] keeps that exact hashing but skips the work for subtrees
//! that did not change:
//!
//! * Writers call [`IncrementalTrie::mark_dirty`] with the changed canonical
//!   key — cheap, no hashing — and the recompute is **batched** to the next
//!   [`IncrementalTrie::root`] call (never per-write).
//! * [`IncrementalTrie::root`] mirrors `merkle_for_node` but, at each node,
//!   first asks "does any dirty key fall in this subtree's key range?". If not,
//!   and the subtree's hash is cached, it returns the cached hash **without
//!   recursing or hashing** — pruning the whole clean subtree in `O(log d)`.
//!
//! Only the hashing primitives are shared with [`crate::trie`]; the node merkle
//! rules are *not* duplicated, so the produced root is identical to
//! `trie_root` by construction. The structural skeleton (range partitioning,
//! path compression) is mirrored and guarded by a differential check against
//! `trie_root` in debug builds and the fuzz targets.
//!
//! ## Cache key and eviction
//!
//! A node is identified by its **routing nibble prefix** `P` (the `depth`
//! nibbles consumed to reach it): the subtree at `P` is exactly the keys whose
//! nibbles start with `P`, so `P` pins both its content and its position.
//!
//! Correctness reduces to one rule: **when key `K` is dirty, evict every
//! prefix of `K` from the cache.** A cached hash for `P` is stale iff the
//! subtree under `P` changed since it was cached, i.e. iff some dirty key `K`
//! has `P` as a prefix — and that commit evicts `P` (it is one of `K`'s
//! prefixes). The eviction happens at dirty time regardless of the current tree
//! shape, so it also covers a node that dissolved, had a descendant change
//! while gone, and later reappeared with the same prefix. A cache hit therefore
//! always means "content unchanged since cached" — no per-node dirty check is
//! needed during the walk.

use crate::trie::{bytes_to_nibbles, compute_node_merkle, compute_path_merkle, MptValue, CHILDREN};
use crate::types::{H256, MERKLE_NULL_NODE};
use rayon::prelude::*;
use rustc_hash::FxHashMap;
use std::collections::{BTreeMap, BTreeSet};

const PARALLEL_REBUILD_THRESHOLD: usize = 4096;
const PARALLEL_REBUILD_DEPTH: usize = 3;

/// Cache keyed by a node's routing nibble prefix. FxHash, not SipHash: these
/// short byte (nibble) keys are hashed on every node visit and SipHash showed
/// up prominently in profiles.
pub(crate) type Cache = FxHashMap<Vec<u8>, H256>;

/// Controls which subtree hashes are retained after a root walk.
///
/// Delta tries are small and hot, so they use the full cache. Snapshot tries can
/// contain millions of singleton subtrees whose cached hashes are rarely reused;
/// skipping those entries keeps branch/path caches warm while avoiding most of
/// the snapshot cache memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CachePolicy {
    Full,
    SkipSingleton,
}

#[cfg(feature = "snapshot-lmdb")]
pub(crate) fn rebuild_snapshot_root_cache(
    snapshot: &BTreeMap<Vec<u8>, Box<[u8]>>,
    cache_policy: CachePolicy,
) -> (H256, Cache) {
    if snapshot.is_empty() {
        return (MERKLE_NULL_NODE, Cache::default());
    }

    let encoded: Vec<(Vec<u8>, &[u8])> = snapshot
        .iter()
        .map(|(key, value)| (bytes_to_nibbles(key), value.as_ref()))
        .collect();
    let entries: Vec<EntryRef<'_>> = encoded
        .iter()
        .map(|(key, value)| EntryRef {
            key: key.as_slice(),
            value: EntryValueRef::Live(value),
        })
        .collect();
    let (root, cache_entries) =
        memo_node_rebuild(&entries, &[], false, cache_policy, PARALLEL_REBUILD_DEPTH);
    let mut cache = Cache::default();
    cache.reserve(cache_entries.len());
    cache.extend(cache_entries);
    (root, cache)
}

/// Incremental delta-trie root.
///
/// Owns the delta as a persistent **nibble-keyed** map, so a commit never
/// re-derives nibbles or rebuilds the entry list for the whole delta — only
/// changed keys are converted, once, at write time. Combined with the
/// prefix-eviction cache, a commit costs roughly `O(changed keys × depth)`
/// instead of `O(delta size)`.
#[derive(Clone, Debug, Default)]
pub struct IncrementalTrie {
    /// Delta entries keyed by nibbles (the trie's working set).
    entries: BTreeMap<Vec<u8>, MptValue>,
    /// Cached subtree hashes, keyed by node routing prefix.
    cache: Cache,
    /// Nibble keys changed since the last `root`, pending prefix eviction.
    dirty: BTreeSet<Vec<u8>>,
}

impl IncrementalTrie {
    /// Build from an existing byte-keyed delta (e.g. a persisted state): one
    /// nibble conversion per entry. The cache starts empty, so the first
    /// `root` is a full (correct) recompute that populates it.
    pub fn from_delta(delta: &BTreeMap<Vec<u8>, MptValue>) -> Self {
        let entries = delta
            .iter()
            .map(|(k, v)| (bytes_to_nibbles(k), v.clone()))
            .collect();
        Self {
            entries,
            cache: Cache::default(),
            dirty: BTreeSet::new(),
        }
    }

    /// Insert / overwrite the value for `key` (the byte-form delta-mpt key).
    /// The nibble conversion happens here, once — not on every commit.
    pub fn insert(&mut self, key: &[u8], value: MptValue) {
        let nibbles = bytes_to_nibbles(key);
        self.dirty.insert(nibbles.clone());
        self.entries.insert(nibbles, value);
    }

    /// Remove `key` (byte-form) from the delta.
    pub fn remove(&mut self, key: &[u8]) {
        let nibbles = bytes_to_nibbles(key);
        self.entries.remove(&nibbles);
        self.dirty.insert(nibbles);
    }

    /// Drop all entries, cached hashes and pending dirty keys. Called when the
    /// delta is reset (e.g. snapshot rotation).
    pub fn clear(&mut self) {
        self.entries.clear();
        self.cache.clear();
        self.dirty.clear();
    }

    /// Merkle root of the current delta, reusing cached hashes for unchanged
    /// subtrees. Clears the pending dirty set; keeps the cache.
    pub fn root(&mut self) -> H256 {
        self.root_with_policy(CachePolicy::Full)
    }

    pub(crate) fn root_with_policy(&mut self, cache_policy: CachePolicy) -> H256 {
        if self.entries.is_empty() {
            self.cache.clear();
            self.dirty.clear();
            return MERKLE_NULL_NODE;
        }
        // Evict every prefix of every dirty key, so no stale subtree hash can
        // survive (see module docs). `dirty` already holds nibble keys.
        for nibbles in &self.dirty {
            for len in 0..=nibbles.len() {
                self.cache.remove(&nibbles[0..len]);
            }
        }
        self.dirty.clear();

        let result = if self.cache.is_empty() && self.entries.len() >= PARALLEL_REBUILD_THRESHOLD {
            let entries: Vec<EntryRef<'_>> = self
                .entries
                .iter()
                .map(|(key, value)| EntryRef {
                    key: key.as_slice(),
                    value: EntryValueRef::Mpt(value),
                })
                .collect();
            let (root, cache_entries) =
                memo_node_rebuild(&entries, &[], false, cache_policy, PARALLEL_REBUILD_DEPTH);
            self.cache.reserve(cache_entries.len());
            self.cache.extend(cache_entries);
            root
        } else {
            memo_node(&self.entries, &[], false, &mut self.cache, cache_policy)
        };
        #[cfg(feature = "verify-incremental")]
        {
            // Cross-check the cached/incremental root against a from-scratch
            // stateless `trie_root` over the SAME entries. This catches any cache
            // or prefix-eviction bug (a stale cached subtree hash surviving when
            // it should have been evicted) — the one failure mode the incremental
            // walk has that the stateless one cannot. Covers BOTH the delta and
            // snapshot incremental tries. O(N) per call: verify builds only.
            let oracle: Vec<(Vec<u8>, &MptValue)> =
                self.entries.iter().map(|(k, v)| (k.clone(), v)).collect();
            assert_eq!(
                result,
                crate::trie::root_for_entries(&oracle),
                "incremental root diverged from stateless trie_root over identical entries"
            );
        }
        result
    }

    /// Build from a canonical snapshot map (all live values, no tombstones):
    /// one nibble conversion per entry. Used to represent the snapshot trie,
    /// which is read-only during a period and re-merged at boundaries. The cache
    /// starts empty, so the first `root` is a full recompute that populates it;
    /// later (incremental) merges clone that warm cache.
    pub fn from_snapshot(snapshot: &BTreeMap<Vec<u8>, Box<[u8]>>) -> Self {
        let entries = snapshot
            .iter()
            .map(|(k, v)| (bytes_to_nibbles(k), MptValue::Some(v.clone())))
            .collect();
        Self {
            entries,
            cache: Cache::default(),
            dirty: BTreeSet::new(),
        }
    }

    /// Point lookup by canonical byte key (snapshot read path). Returns the live
    /// value bytes, or `None` if absent.
    pub fn snapshot_get(&self, canonical: &[u8]) -> Option<&[u8]> {
        self.entries
            .get(&bytes_to_nibbles(canonical))
            .and_then(|v| v.visible_value())
    }

    /// All `(canonical key, value)` pairs whose canonical key starts with
    /// `canonical_prefix` (snapshot prefix scan). Keys are converted back from
    /// nibble form; snapshot keys are byte-aligned so nibble length is even.
    pub fn snapshot_scan_prefix(&self, canonical_prefix: &[u8]) -> Vec<(Vec<u8>, Box<[u8]>)> {
        let nibble_prefix = bytes_to_nibbles(canonical_prefix);
        let upper = prefix_upper(&nibble_prefix);
        self.entries
            .range(nibble_prefix..upper)
            .filter_map(|(nibbles, value)| {
                value
                    .visible_value()
                    .map(|v| (nibbles_to_bytes(nibbles), Box::from(v)))
            })
            .collect()
    }

    /// Flatten back to a canonical byte-keyed map (for checkpoint persistence).
    pub fn to_canonical_map(&self) -> BTreeMap<Vec<u8>, Box<[u8]>> {
        self.entries
            .iter()
            .filter_map(|(nibbles, value)| {
                value
                    .visible_value()
                    .map(|v| (nibbles_to_bytes(nibbles), Box::from(v)))
            })
            .collect()
    }

    /// Number of stored entries (used by merge instrumentation).
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Inverse of [`bytes_to_nibbles`] for byte-aligned (even-length) nibble keys:
/// pairs of nibbles recombine into bytes. Snapshot keys are always byte-aligned.
pub(crate) fn nibbles_to_bytes(nibbles: &[u8]) -> Vec<u8> {
    nibbles
        .chunks_exact(2)
        .map(|p| (p[0] << 4) | p[1])
        .collect()
}

/// Exclusive upper bound for the BTreeMap range covering all keys with routing
/// prefix `rp`: `rp` followed by `CHILDREN` (`16`), which is strictly greater
/// than every real nibble (`0..16`). So the half-open range `[rp, rp++[16])` is
/// exactly the keys prefixed by `rp`.
pub(crate) fn prefix_upper(rp: &[u8]) -> Vec<u8> {
    let mut upper = Vec::with_capacity(rp.len() + 1);
    upper.extend_from_slice(rp);
    upper.push(CHILDREN as u8);
    upper
}

/// Range-driven mirror of [`crate::trie`]'s `merkle_for_node`, plus a cache keyed
/// by the node's routing prefix `rp`. Instead of materialising the whole delta
/// into a slice and scanning it, the node's key block is addressed directly on
/// the sorted `entries` map via `range` queries, and clean children short-circuit
/// on a cache hit without ever touching `entries`. A hit is always valid because
/// `root` evicted any prefix touched by a dirty key (see module docs).
///
/// Cost is `O(visited nodes × CHILDREN × log N)` — proportional to the dirty
/// frontier, not the delta size: the old slice version paid `O(N)` per call just
/// to flatten the map and re-partition the root level (always dirty). Serial only
/// (the cache is `&mut`).
fn memo_node(
    entries: &BTreeMap<Vec<u8>, MptValue>,
    rp: &[u8],
    allow_path_compression: bool,
    cache: &mut Cache,
    cache_policy: CachePolicy,
) -> H256 {
    if let Some(hash) = cache.get(rp) {
        return *hash;
    }
    let depth = rp.len();
    let upper = prefix_upper(rp);

    // The node's key block is the contiguous range `[rp, rp++[16])`. It is
    // non-empty: the root is non-empty and `root`/this fn only descend into
    // children proven to exist. The first key carries the routing/compression
    // nibbles; with path compression the node absorbs the longest prefix shared
    // by the whole block, which — the block being sorted — is the common prefix
    // of just its first (min) and last (max) key.
    let first_key = entries
        .range(rp.to_vec()..upper.clone())
        .next()
        .expect("non-empty node")
        .0;
    let last_key = if allow_path_compression || cache_policy == CachePolicy::SkipSingleton {
        entries
            .range(rp.to_vec()..upper)
            .next_back()
            .expect("non-empty node")
            .0
    } else {
        first_key
    };
    let is_singleton = first_key == last_key;
    let common = if allow_path_compression {
        common_prefix_len(first_key, last_key, depth)
    } else {
        0
    };
    let node_depth = depth + common;
    let full_prefix = &first_key[0..node_depth];

    // A key ending exactly at this node can only be `full_prefix` itself.
    let maybe_value = entries.get(full_prefix).map(|value| value.merkle_value());

    let mut children: [H256; CHILDREN] = [MERKLE_NULL_NODE; CHILDREN];
    for (c, slot) in children.iter_mut().enumerate() {
        let mut child_rp = Vec::with_capacity(node_depth + 1);
        child_rp.extend_from_slice(full_prefix);
        child_rp.push(c as u8);
        if let Some(hash) = cache.get(&child_rp) {
            // Clean subtree: reuse the cached hash without touching `entries`.
            *slot = *hash;
            continue;
        }
        // Cache miss: the child is either dirty (present) or absent. One range
        // probe decides; only present children are recomputed.
        let child_upper = prefix_upper(&child_rp);
        if entries
            .range(child_rp.clone()..child_upper)
            .next()
            .is_some()
        {
            *slot = memo_node(entries, &child_rp, true, cache, cache_policy);
        }
    }

    let has_children = children.iter().any(|h| *h != MERKLE_NULL_NODE);
    let node_hash = compute_node_merkle(has_children.then_some(&children), maybe_value);
    let hash = compute_path_merkle(&first_key[depth..node_depth], depth, node_hash);

    if cache_policy == CachePolicy::Full || !is_singleton {
        cache.insert(rp.to_vec(), hash);
    }
    hash
}

#[derive(Clone, Copy)]
struct EntryRef<'a> {
    key: &'a [u8],
    value: EntryValueRef<'a>,
}

#[derive(Clone, Copy)]
enum EntryValueRef<'a> {
    Mpt(&'a MptValue),
    #[cfg(feature = "snapshot-lmdb")]
    Live(&'a [u8]),
}

impl<'a> EntryValueRef<'a> {
    fn merkle_value(self) -> &'a [u8] {
        match self {
            Self::Mpt(value) => value.merkle_value(),
            #[cfg(feature = "snapshot-lmdb")]
            Self::Live(value) => value,
        }
    }
}

/// Cold-cache root/cache rebuild over an already sorted entry slice.
///
/// This mirrors `memo_node`, but it uses contiguous slice partitions instead of
/// BTreeMap range probes. Because the caller only uses it when the cache is
/// empty, children return cache entries as vectors; the caller builds the real
/// hash map once after all parallel work joins.
fn memo_node_rebuild(
    entries: &[EntryRef<'_>],
    rp: &[u8],
    allow_path_compression: bool,
    cache_policy: CachePolicy,
    parallel_depth: usize,
) -> (H256, Vec<(Vec<u8>, H256)>) {
    debug_assert!(!entries.is_empty());

    let depth = rp.len();
    let first_key = entries[0].key;
    let last_key = entries[entries.len() - 1].key;
    let is_singleton = first_key == last_key;
    let common = if allow_path_compression {
        common_prefix_len(first_key, last_key, depth)
    } else {
        0
    };
    let node_depth = depth + common;
    let full_prefix = &first_key[0..node_depth];

    let maybe_value = entries
        .iter()
        .find(|entry| entry.key.len() == node_depth)
        .map(|entry| entry.value.merkle_value());

    let ranges = child_ranges(entries, node_depth);
    let (child_hashes, mut cache_entries): (Vec<(usize, H256)>, Vec<(Vec<u8>, H256)>) =
        if parallel_depth > 0 && entries.len() >= PARALLEL_REBUILD_THRESHOLD && ranges.len() > 1 {
            let results: Vec<(usize, H256, Vec<(Vec<u8>, H256)>)> = ranges
                .par_iter()
                .map(|(child, start, end)| {
                    let mut child_rp = Vec::with_capacity(node_depth + 1);
                    child_rp.extend_from_slice(full_prefix);
                    child_rp.push(*child as u8);

                    let (hash, cache_entries) = memo_node_rebuild(
                        &entries[*start..*end],
                        &child_rp,
                        true,
                        cache_policy,
                        parallel_depth - 1,
                    );
                    (*child, hash, cache_entries)
                })
                .collect();

            let mut child_hashes = Vec::with_capacity(results.len());
            let cache_entries_len = results
                .iter()
                .map(|(_, _, cache_entries)| cache_entries.len())
                .sum();
            let mut cache_entries = Vec::with_capacity(cache_entries_len);
            for (child, hash, mut child_cache_entries) in results {
                cache_entries.append(&mut child_cache_entries);
                child_hashes.push((child, hash));
            }
            (child_hashes, cache_entries)
        } else {
            let mut child_hashes = Vec::with_capacity(ranges.len());
            let mut cache_entries = Vec::new();
            for (child, start, end) in &ranges {
                let mut child_rp = Vec::with_capacity(node_depth + 1);
                child_rp.extend_from_slice(full_prefix);
                child_rp.push(*child as u8);
                let (hash, mut child_cache_entries) =
                    memo_node_rebuild(&entries[*start..*end], &child_rp, true, cache_policy, 0);
                cache_entries.append(&mut child_cache_entries);
                child_hashes.push((*child, hash));
            }
            (child_hashes, cache_entries)
        };

    let mut children: [H256; CHILDREN] = [MERKLE_NULL_NODE; CHILDREN];
    for (child, hash) in child_hashes {
        children[child] = hash;
    }

    let has_children = children.iter().any(|h| *h != MERKLE_NULL_NODE);
    let node_hash = compute_node_merkle(has_children.then_some(&children), maybe_value);
    let hash = compute_path_merkle(&first_key[depth..node_depth], depth, node_hash);

    if cache_policy == CachePolicy::Full || !is_singleton {
        cache_entries.push((rp.to_vec(), hash));
    }
    (hash, cache_entries)
}

fn child_ranges(entries: &[EntryRef<'_>], node_depth: usize) -> Vec<(usize, usize, usize)> {
    let mut ranges = Vec::new();
    let mut idx = 0;
    while idx < entries.len() {
        let key = entries[idx].key;
        if key.len() == node_depth {
            idx += 1;
            continue;
        }

        let child = key[node_depth] as usize;
        let start = idx;
        idx += 1;
        while idx < entries.len()
            && entries[idx].key.len() > node_depth
            && entries[idx].key[node_depth] as usize == child
        {
            idx += 1;
        }
        ranges.push((child, start, idx));
    }
    ranges
}

/// Longest common nibble prefix of the sorted block beyond `depth`. The block's
/// LCP equals the LCP of its endpoints `first` (min) and `last` (max), so only
/// the two boundary keys are compared.
pub(crate) fn common_prefix_len(first: &[u8], last: &[u8], depth: usize) -> usize {
    first
        .iter()
        .zip(last)
        .skip(depth)
        .take_while(|(a, b)| a == b)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trie::trie_root;

    fn val(b: u8) -> MptValue {
        if b == 0 {
            MptValue::Tombstone
        } else {
            MptValue::Some(Box::from([b]))
        }
    }

    /// Drive a map through a random op sequence, asserting the incremental root
    /// equals the stateless oracle after every change.
    #[test]
    fn matches_oracle_under_random_ops() {
        // Small deterministic LCG so the test is reproducible without deps.
        let mut seed: u64 = 0x9e3779b97f4a7c15;
        let mut next = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            (seed >> 33) as u32
        };

        let mut map: BTreeMap<Vec<u8>, MptValue> = BTreeMap::new();
        let mut inc = IncrementalTrie::default();

        for op_i in 0..4000 {
            // Keys share prefixes to exercise path compression split/merge.
            let klen = 1 + (next() % 4) as usize;
            let mut key = Vec::with_capacity(klen);
            for _ in 0..klen {
                key.push((next() % 6) as u8);
            }
            let v = (next() % 4) as u8;

            let action = if v == 0 && map.contains_key(&key) {
                map.remove(&key);
                inc.remove(&key);
                "remove"
            } else {
                map.insert(key.clone(), val(v));
                inc.insert(&key, val(v));
                "insert"
            };

            // Mostly batch several ops before comparing (exercising multi-dirty
            // commits), but compare often enough to localise any divergence.
            if next() % 3 == 0 {
                assert_eq!(
                    inc.root(),
                    trie_root(&map),
                    "diverged at op {op_i}: {action} key={key:?} size {}",
                    map.len()
                );
            }
        }
        assert_eq!(inc.root(), trie_root(&map));
    }

    /// Same, but compare after *every* op so a single-op update bug cannot hide
    /// behind a later full recompute.
    #[test]
    fn matches_oracle_every_op() {
        let mut seed: u64 = 0x2545f4914f6cdd1d;
        let mut next = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            (seed >> 33) as u32
        };
        let mut map: BTreeMap<Vec<u8>, MptValue> = BTreeMap::new();
        let mut inc = IncrementalTrie::default();
        for op_i in 0..4000 {
            let klen = 1 + (next() % 5) as usize;
            let key: Vec<u8> = (0..klen).map(|_| (next() % 6) as u8).collect();
            let v = (next() % 4) as u8;
            if v == 0 && map.contains_key(&key) {
                map.remove(&key);
                inc.remove(&key);
            } else {
                map.insert(key.clone(), val(v));
                inc.insert(&key, val(v));
            }
            assert_eq!(
                inc.root(),
                trie_root(&map),
                "diverged at op {op_i} key={key:?}"
            );
        }
    }

    #[test]
    fn empty_after_clear_is_null() {
        let mut inc = IncrementalTrie::default();
        let mut map = BTreeMap::new();
        map.insert(vec![1u8], MptValue::Some(Box::from([1u8])));
        inc.insert(&[1u8], MptValue::Some(Box::from([1u8])));
        assert_eq!(inc.root(), trie_root(&map));
        inc.clear();
        assert_eq!(inc.root(), MERKLE_NULL_NODE);
    }

    /// `from_delta` reproduces the oracle for a preloaded delta.
    #[test]
    fn from_delta_matches_oracle() {
        let mut map: BTreeMap<Vec<u8>, MptValue> = BTreeMap::new();
        for b in 0u8..50 {
            map.insert(vec![b % 7, b % 5, b], val(b | 1));
        }
        let mut inc = IncrementalTrie::from_delta(&map);
        assert_eq!(inc.root(), trie_root(&map));
    }

    /// Micro-benchmark (run: `cargo test --release -p cfx-minimal-mpt \
    /// bench_incremental -- --nocapture --ignored`). Realistic delta of wide
    /// random 40-byte keys; per round modify a few keys then take the root.
    #[test]
    #[ignore]
    fn bench_incremental_vs_oracle() {
        use std::time::Instant;
        fn lcg(s: &mut u64) -> u64 {
            *s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
            *s >> 33
        }
        let mut seed = 0xdead_beef_u64;
        for &n in &[2000usize, 6000] {
            let mut keys: Vec<Vec<u8>> = Vec::with_capacity(n);
            let mut map: BTreeMap<Vec<u8>, MptValue> = BTreeMap::new();
            let mut inc = IncrementalTrie::default();
            for _ in 0..n {
                let key: Vec<u8> = (0..40).map(|_| (lcg(&mut seed) & 0xff) as u8).collect();
                inc.insert(&key, MptValue::Some(Box::from([1u8])));
                map.insert(key.clone(), MptValue::Some(Box::from([1u8])));
                keys.push(key);
            }
            inc.root(); // warm

            let rounds = 300usize;
            let mut s = seed;
            let inc_t = Instant::now();
            for _ in 0..rounds {
                for _ in 0..4 {
                    s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
                    let k = &keys[(s >> 33) as usize % keys.len()];
                    let v = MptValue::Some(Box::from([(s >> 40) as u8]));
                    inc.insert(k, v.clone());
                    map.insert(k.clone(), v);
                }
                let _ = inc.root();
            }
            let inc_dur = inc_t.elapsed();

            let mut s = seed;
            let or_t = Instant::now();
            for _ in 0..rounds {
                for _ in 0..4 {
                    s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
                    let k = &keys[(s >> 33) as usize % keys.len()];
                    map.insert(k.clone(), MptValue::Some(Box::from([(s >> 40) as u8])));
                }
                let _ = trie_root(&map);
            }
            let or_dur = or_t.elapsed();

            eprintln!(
                "N={n} rounds={rounds}: incremental={inc_dur:?} oracle={or_dur:?} \
                 speedup={:.2}x  (per-commit inc={:?} oracle={:?})",
                or_dur.as_secs_f64() / inc_dur.as_secs_f64(),
                inc_dur / rounds as u32,
                or_dur / rounds as u32,
            );
        }
    }
}
