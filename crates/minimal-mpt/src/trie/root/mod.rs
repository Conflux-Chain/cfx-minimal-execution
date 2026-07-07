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
//! ## Key representation
//!
//! Keys are byte-aligned, so their **nibble length is even** and packing two
//! nibbles per byte reproduces the canonical byte key exactly. The entries map
//! is therefore keyed directly on canonical bytes (`Box<[u8]>`) — half the heap
//! of the old one-nibble-per-byte form — and nibble-level routing goes through
//! the zero-cost [`Nibbles`] view. Routing prefixes / cache keys can be odd
//! length, so they use [`NibblePrefix`] (packed + a parity byte). See
//! [`crate::nibbles`].
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

use crate::persist::wire::{read_len_prefixed, read_u64_le};
use crate::entries::{Composite, Entries};
use crate::nibbles::{
    empty_prefix_buf, pack_nibbles, push_nibble, NibblePrefix, NibbleRef, Nibbles,
};
use crate::trie::{compute_node_merkle, compute_path_merkle, MptValue, TrieValue, CHILDREN};
use crate::primitives::{H256, MERKLE_NULL_NODE};
use rayon::prelude::*;
use rustc_hash::FxHashMap;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Read};

pub(crate) const PARALLEL_REBUILD_THRESHOLD: usize = 4096;

/// Cache keyed by a node's routing nibble prefix. FxHash, not SipHash: these
/// short packed keys are hashed on every node visit and SipHash showed up
/// prominently in profiles.
pub(crate) type Cache = FxHashMap<NibblePrefix, H256>;

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

/// Incremental delta-trie root.
///
/// Owns the delta as a persistent **canonical-byte-keyed** map, so a commit
/// never rebuilds the entry list for the whole delta — only changed keys are
/// touched. Combined with the prefix-eviction cache, a commit costs roughly
/// `O(changed keys × depth)` instead of `O(delta size)`.
///
/// Generic over the stored value type `V` (default [`MptValue`]): the delta and
/// intermediate tries keep `MptValue` (which carries `Tombstone`), while the
/// snapshot — provably tombstone-free — instantiates it with a compact inline
/// value type to save memory. Only [`TrieValue`] (merkle bytes + visibility) is
/// required, so the hashing and cache logic are identical across value types.
#[derive(Clone, Debug)]
pub struct IncrementalTrie<V = MptValue> {
    /// Delta entries, stored two-level to factor out shared key prefixes.
    entries: Entries<V>,
    /// Cached subtree hashes, keyed by node routing prefix.
    cache: Cache,
    /// Canonical keys changed since the last `root`, pending prefix eviction.
    dirty: BTreeSet<Box<[u8]>>,
}

impl<V> Default for IncrementalTrie<V> {
    fn default() -> Self {
        Self {
            entries: Entries::new(),
            cache: Cache::default(),
            dirty: BTreeSet::new(),
        }
    }
}

impl IncrementalTrie<MptValue> {
    /// Build from an existing byte-keyed delta (e.g. a persisted state). The
    /// cache starts empty, so the first `root` is a full (correct) recompute
    /// that populates it. Delta-only, so it keeps `MptValue` (tombstones are
    /// meaningful in the delta).
    pub fn from_delta(delta: &BTreeMap<Vec<u8>, MptValue>) -> Self {
        let mut entries = Entries::new();
        for (k, v) in delta {
            entries.insert(k, v.clone());
        }
        Self {
            entries,
            cache: Cache::default(),
            dirty: BTreeSet::new(),
        }
    }
}

impl<V: TrieValue> IncrementalTrie<V> {
    /// Insert / overwrite the value for `key` (the canonical byte mpt key).
    pub fn insert(&mut self, key: &[u8], value: V) {
        self.dirty.insert(Box::from(key));
        self.entries.insert(key, value);
    }

    /// Remove `key` (canonical byte form) from the delta.
    pub fn remove(&mut self, key: &[u8]) {
        self.entries.remove(key);
        self.dirty.insert(Box::from(key));
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
        // survive (see module docs). Each key's prefixes are built by extending a
        // single reused stack buffer one nibble at a time (no per-nibble alloc),
        // and the cache is evicted by the borrowed `&[u8]` key.
        for key in &self.dirty {
            let mut buf = empty_prefix_buf();
            self.cache.remove(&buf[..]); // the empty (root) prefix
            for &byte in key.iter() {
                push_nibble(&mut buf, byte >> 4);
                self.cache.remove(&buf[..]);
                push_nibble(&mut buf, byte & 0x0f);
                self.cache.remove(&buf[..]);
            }
        }
        self.dirty.clear();

        let result = if self.cache.is_empty() && self.entries.len() >= PARALLEL_REBUILD_THRESHOLD {
            let timing = std::env::var_os("MMPT_RECOVER_TIMING").is_some();
            let entries: Vec<EntryRef<'_>> = self
                .entries
                .iter()
                .map(|(key, value)| EntryRef {
                    key,
                    value: value.merkle_value(),
                })
                .collect();
            let t = std::time::Instant::now();
            let (root, cache_entries) =
                memo_node_rebuild(&entries, NibbleRef::empty(), false, cache_policy);
            if timing {
                eprintln!(
                    "[recover] snapshot_hash elapsed={}ms cache_entries={}",
                    t.elapsed().as_millis(),
                    cache_entries.len()
                );
            }
            let t = std::time::Instant::now();
            self.cache.reserve(cache_entries.len());
            self.cache.extend(cache_entries);
            if timing {
                eprintln!(
                    "[recover] snapshot_cache_extend elapsed={}ms cache_len={}",
                    t.elapsed().as_millis(),
                    self.cache.len()
                );
            }
            root
        } else {
            memo_node(
                &self.entries,
                NibbleRef::empty(),
                false,
                &mut self.cache,
                cache_policy,
            )
        };
        #[cfg(feature = "verify-incremental")]
        {
            // Cross-check the cached/incremental root against a from-scratch
            // stateless `trie_root` over the SAME entries. This catches any cache
            // or prefix-eviction bug (a stale cached subtree hash surviving when
            // it should have been evicted) — the one failure mode the incremental
            // walk has that the stateless one cannot. Covers BOTH the delta and
            // snapshot incremental tries. O(N) per call: verify builds only.
            // The oracle works in expanded nibbles, so re-derive them from the
            // canonical keys (the snapshot's value carries no tombstone).
            let oracle_owned: Vec<(Vec<u8>, MptValue)> = self
                .entries
                .iter()
                .map(|(c, v)| {
                    let key: Vec<u8> = [c.head, c.tail].concat();
                    let nibbles = crate::trie::bytes_to_nibbles(&key);
                    let mv = match v.visible_value() {
                        Some(b) => MptValue::Some(Box::from(b)),
                        None => MptValue::Tombstone,
                    };
                    (nibbles, mv)
                })
                .collect();
            let oracle: Vec<(Vec<u8>, &MptValue)> =
                oracle_owned.iter().map(|(k, v)| (k.clone(), v)).collect();
            assert_eq!(
                result,
                crate::trie::root_for_entries(&oracle),
                "incremental root diverged from stateless trie_root over identical entries"
            );
        }
        result
    }

    /// Build from a canonical snapshot map (all live values, no tombstones).
    /// Used to represent the snapshot trie, which is read-only during a period
    /// and re-merged at boundaries. The cache starts empty, so the first `root`
    /// is a full recompute that populates it; later (incremental) merges clone
    /// that warm cache.
    pub fn from_snapshot(snapshot: &BTreeMap<Vec<u8>, Box<[u8]>>) -> Self {
        let mut entries = Entries::new();
        for (k, v) in snapshot {
            entries.insert(k, V::from_value_bytes(v));
        }
        Self {
            entries,
            cache: Cache::default(),
            dirty: BTreeSet::new(),
        }
    }

    /// Stream a snapshot trie from a bincode-encoded
    /// `BTreeMap<Vec<u8>, Box<[u8]>>` (`u64 count` then `count` × `(key, value)`
    /// length-prefixed) without ever materializing an intermediate map: each
    /// entry is inserted (canonical key, converted value) as it is read. Cache
    /// starts empty; the first `root` recomputes it.
    pub(crate) fn from_snapshot_reader(reader: &mut impl Read) -> io::Result<Self> {
        let count = read_u64_le(reader)? as usize;
        // On-disk keys are ascending, so build the store densely: read fixed-size
        // sorted batches of `W` and hand each to `bulk_extend_sorted` (dense
        // level-2 leaves) instead of monotonic per-entry `insert` (~half-full
        // leaves). The reader stays oblivious to the two-level layout; peak
        // transient is one batch. `W` is env-tunable for profiling.
        let w = std::env::var("MMPT_BULK_W")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|&w| w > 0)
            .unwrap_or(1_000_000);
        let mut entries = Entries::new();
        let mut batch: Vec<(Vec<u8>, V)> = Vec::with_capacity(w.min(count));
        for _ in 0..count {
            let key = read_len_prefixed(reader)?;
            let value = read_len_prefixed(reader)?;
            batch.push((key, V::from_value_bytes(&value)));
            if batch.len() >= w {
                entries.bulk_extend_sorted(std::mem::replace(&mut batch, Vec::with_capacity(w)));
            }
        }
        if !batch.is_empty() {
            entries.bulk_extend_sorted(batch);
        }
        Ok(Self {
            entries,
            cache: Cache::default(),
            dirty: BTreeSet::new(),
        })
    }

    /// Point lookup by canonical byte key (snapshot read path). Returns the live
    /// value bytes, or `None` if absent.
    pub fn snapshot_get(&self, canonical: &[u8]) -> Option<&[u8]> {
        self.entries.get(canonical).and_then(|v| v.visible_value())
    }

    /// All `(canonical key, value)` pairs whose canonical key starts with
    /// `canonical_prefix` (snapshot prefix scan).
    pub fn snapshot_scan_prefix(&self, canonical_prefix: &[u8]) -> Vec<(Vec<u8>, Box<[u8]>)> {
        let buf = pack_nibbles(Nibbles::from_bytes(canonical_prefix));
        self.entries
            .prefix_range(NibbleRef::new(&buf))
            .filter_map(|(c, value)| {
                value
                    .visible_value()
                    .map(|v| ([c.head, c.tail].concat(), Box::from(v)))
            })
            .collect()
    }

    /// Flatten to a canonical byte-keyed map (for checkpoint persistence).
    pub fn to_canonical_map(&self) -> BTreeMap<Vec<u8>, Box<[u8]>> {
        self.entries
            .iter()
            .filter_map(|(c, value)| {
                value
                    .visible_value()
                    .map(|v| ([c.head, c.tail].concat(), Box::from(v)))
            })
            .collect()
    }

    /// Number of live (non-tombstone) entries.
    pub fn live_entry_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|(_, v)| v.visible_value().is_some())
            .count()
    }

    /// Iterate live entries as `(canonical_byte_key, value_bytes)` in sorted
    /// order. Each key is reassembled from its `head ++ tail` on the fly.
    pub fn iter_canonical(&self) -> impl Iterator<Item = (Vec<u8>, &[u8])> + '_ {
        self.entries
            .iter()
            .filter_map(|(c, value)| value.visible_value().map(|v| ([c.head, c.tail].concat(), v)))
    }

    /// Number of stored entries (used by merge instrumentation).
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Range-driven mirror of [`crate::trie`]'s `merkle_for_node`, plus a cache keyed
/// by the node's routing prefix `rp`. Instead of materialising the whole delta
/// into a slice and scanning it, the node's key block is addressed directly on
/// the sorted `entries` map via `range` queries, and clean children short-circuit
/// on a cache hit without ever touching `entries`. A hit is always valid because
/// `root` evicted any prefix touched by a dirty key (see module docs).
///
/// Cost is `O(visited nodes × CHILDREN × log N)` — proportional to the dirty
/// frontier, not the delta size. Serial only (the cache is `&mut`).
fn memo_node<V: TrieValue>(
    entries: &Entries<V>,
    rp: NibbleRef,
    allow_path_compression: bool,
    cache: &mut Cache,
    cache_policy: CachePolicy,
) -> H256 {
    if let Some(hash) = cache.get(rp.as_bytes()) {
        return *hash;
    }
    let depth = rp.nibble_len();

    // The node's key block is the contiguous key range for `rp`. It is non-empty:
    // the root is non-empty and this fn only descends into children proven to
    // exist. The first key carries the routing/compression nibbles; with path
    // compression the node absorbs the longest prefix shared by the whole block,
    // which — the block being sorted — is the common prefix of just its first
    // (min) and last (max) key.
    let first_key = entries.prefix_first(rp).expect("non-empty node");
    let last_key = if allow_path_compression || cache_policy == CachePolicy::SkipSingleton {
        entries.prefix_last(rp).expect("non-empty node")
    } else {
        first_key
    };
    let first = first_key.nibbles();
    let last = last_key.nibbles();
    let is_singleton = first_key.same_key(&last_key);
    let common = if allow_path_compression {
        first.common_prefix_len_from(&last, depth)
    } else {
        0
    };
    let node_depth = depth + common;
    // The node's full routing prefix, built once on the stack.
    let full_buf = pack_nibbles(first.sub(0, node_depth));
    let full = NibbleRef::new(&full_buf);

    // A key ending exactly at this node can only be `full` itself (and only when
    // it lands on a byte boundary — see `as_canonical_key`).
    let maybe_value = full
        .as_canonical_key()
        .and_then(|ck| entries.get(ck))
        .map(|value| value.merkle_value());

    // Resolve all 16 child slots left-to-right. Each child routing key is `full`
    // extended by one nibble into a fresh stack buffer (no heap): a cache hit
    // reuses the clean subtree's hash; on a miss, one range probe decides — a
    // present (dirty) child recurses, an absent one stays null.
    let children: [H256; CHILDREN] = std::array::from_fn(|c| {
        let mut child_buf = full_buf.clone();
        push_nibble(&mut child_buf, c as u8);
        if let Some(hash) = cache.get(&child_buf[..]) {
            return *hash;
        }
        let child = NibbleRef::new(&child_buf);
        if entries.prefix_exists(child) {
            memo_node(entries, child, true, cache, cache_policy)
        } else {
            MERKLE_NULL_NODE
        }
    });

    let has_children = children.iter().any(|h| *h != MERKLE_NULL_NODE);
    let node_hash = compute_node_merkle(has_children.then_some(&children), maybe_value);
    let path = first.sub(depth, node_depth).to_expanded();
    let hash = compute_path_merkle(&path, depth, node_hash);

    if cache_policy == CachePolicy::Full || !is_singleton {
        cache.insert(NibblePrefix::from_bytes(rp.as_bytes()), hash);
    }
    hash
}

/// One entry fed to the cold [`memo_node_rebuild`]: a **canonical byte** key plus
/// its precomputed **merkle bytes** (empty for a tombstone). The rebuild only
/// ever needs the merkle bytes, so callers flatten `TrieValue::merkle_value` up
/// front — keeping the rebuild independent of the value type. Nibble routing is
/// derived from the canonical key via [`Nibbles`].
#[derive(Clone, Copy)]
struct EntryRef<'a> {
    key: Composite<'a>,
    value: &'a [u8],
}

impl<'a> EntryRef<'a> {
    #[inline]
    fn nibbles(&self) -> Nibbles<'a> {
        self.key.nibbles()
    }
    #[inline]
    fn nibble_len(&self) -> usize {
        self.key.byte_len() * 2
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
    rp: NibbleRef,
    allow_path_compression: bool,
    cache_policy: CachePolicy,
) -> (H256, Vec<(NibblePrefix, H256)>) {
    debug_assert!(!entries.is_empty());

    let depth = rp.nibble_len();
    let first = entries[0].nibbles();
    let last = entries[entries.len() - 1].nibbles();
    let is_singleton = entries[0].key.same_key(&entries[entries.len() - 1].key);
    let common = if allow_path_compression {
        first.common_prefix_len_from(&last, depth)
    } else {
        0
    };
    let node_depth = depth + common;
    let full_buf = pack_nibbles(first.sub(0, node_depth));

    let maybe_value = entries
        .iter()
        .find(|entry| entry.nibble_len() == node_depth)
        .map(|entry| entry.value);

    // Recompute one child subtree: build its routing key on the stack, recurse
    // over its slice, and return `(child index, hash, its cache entries)`.
    let recurse = |&(child, start, end): &(usize, usize, usize)| {
        let mut child_buf = full_buf.clone();
        push_nibble(&mut child_buf, child as u8);
        let (hash, cache_entries) =
            memo_node_rebuild(&entries[start..end], NibbleRef::new(&child_buf), true, cache_policy);
        (child, hash, cache_entries)
    };

    // Fan out across cores only for a large, multi-child block; recursion turns
    // serial once a subtree drops below the threshold, bounding task
    // granularity. Splitting by SUBTREE SIZE (not a fixed depth) lets a few
    // oversized subtrees — e.g. a large-storage contract — spread across cores.
    // Nested par_iter is fine: rayon work-steals over the shared pool.
    let ranges = child_ranges(entries, node_depth);
    let results: Vec<(usize, H256, Vec<(NibblePrefix, H256)>)> =
        if entries.len() >= PARALLEL_REBUILD_THRESHOLD && ranges.len() > 1 {
            ranges.par_iter().map(&recurse).collect()
        } else {
            ranges.iter().map(&recurse).collect()
        };

    let mut children: [H256; CHILDREN] = [MERKLE_NULL_NODE; CHILDREN];
    let mut cache_entries = Vec::with_capacity(results.iter().map(|(_, _, c)| c.len()).sum());
    for (child, hash, mut child_cache_entries) in results {
        children[child] = hash;
        cache_entries.append(&mut child_cache_entries);
    }

    let has_children = children.iter().any(|h| *h != MERKLE_NULL_NODE);
    let node_hash = compute_node_merkle(has_children.then_some(&children), maybe_value);
    let path = first.sub(depth, node_depth).to_expanded();
    let hash = compute_path_merkle(&path, depth, node_hash);

    if cache_policy == CachePolicy::Full || !is_singleton {
        cache_entries.push((NibblePrefix::from_bytes(rp.as_bytes()), hash));
    }
    (hash, cache_entries)
}

/// Partition a sorted node block into `(child nibble, start, end)` runs, one per
/// child subtree. Entries whose nibble length equals `node_depth` carry no child
/// nibble and are dropped; the rest — being sorted — group into contiguous runs
/// sharing their `node_depth`-th nibble.
fn child_ranges(entries: &[EntryRef<'_>], node_depth: usize) -> Vec<(usize, usize, usize)> {
    entries
        .iter()
        .enumerate()
        .filter_map(|(idx, entry)| {
            (node_depth < entry.nibble_len()).then(|| (idx, entry.nibbles().at(node_depth) as usize))
        })
        .fold(Vec::new(), |mut ranges, (idx, child)| {
            match ranges.last_mut() {
                Some(last) if last.0 == child && last.2 == idx => last.2 = idx + 1,
                _ => ranges.push((child, idx, idx + 1)),
            }
            ranges
        })
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
            let key: Vec<u8> = (0..klen).map(|_| (next() % 6) as u8).collect();
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

    /// Force the cold `memo_node_rebuild` PARALLEL fan-out (>= threshold entries,
    /// empty cache, >1 child at the root) and assert it agrees with the stateless
    /// oracle — the small-map tests above stay under the serial threshold.
    #[test]
    fn parallel_rebuild_matches_oracle() {
        let mut map: BTreeMap<Vec<u8>, MptValue> = BTreeMap::new();
        for i in 0u32..8000 {
            // First byte = i&0xff spans all 16 top nibbles (root fans out); the
            // first two bytes are a bijection with i, so keys stay distinct.
            let key = vec![(i & 0xff) as u8, (i >> 8) as u8, ((i >> 3) & 0xff) as u8];
            map.insert(key, val((i as u8) | 1));
        }
        assert!(map.len() >= PARALLEL_REBUILD_THRESHOLD);
        let mut inc = IncrementalTrie::from_delta(&map);
        assert_eq!(inc.root(), trie_root(&map));
    }

    /// Same cold parallel-rebuild path, but with tombstones interleaved. Proves
    /// a terminal tombstone still hashes as `Some(&[])` (the `b'v'` marker in
    /// `compute_node_merkle`) — byte-identical to the stateless oracle. The
    /// Some/None distinction is carried by key-presence (`find` on nibble
    /// length), which the flattening to merkle bytes does not touch;
    /// `merkle_value()` collapses tombstone and empty `Some` to the same `&[]`,
    /// exactly as the oracle does.
    #[test]
    fn parallel_rebuild_with_tombstones_matches_oracle() {
        let mut map: BTreeMap<Vec<u8>, MptValue> = BTreeMap::new();
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
            map.insert(key, v);
        }
        assert!(map.len() >= PARALLEL_REBUILD_THRESHOLD);
        let mut inc = IncrementalTrie::from_delta(&map);
        assert_eq!(inc.root(), trie_root(&map));
    }

    /// Wide byte-aligned keys (like real 20/57-byte mpt keys) that produce odd
    /// routing depths and long shared prefixes — exercises the packed
    /// `NibblePrefix` range translation end-to-end against the oracle.
    #[test]
    fn wide_byte_keys_match_oracle() {
        let mut seed: u64 = 0xd1b54a32d192ed03;
        let mut next = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            (seed >> 24) as u8
        };
        let mut map: BTreeMap<Vec<u8>, MptValue> = BTreeMap::new();
        let mut inc = IncrementalTrie::default();
        // A shared 20-byte account prefix + varying storage tails → deep nodes.
        let account: Vec<u8> = (0..20).map(|_| next()).collect();
        for _ in 0..3000 {
            let mut key = account.clone();
            key.extend((0..37).map(|_| next()));
            let v = MptValue::Some(Box::from([next(), next()]));
            inc.insert(&key, v.clone());
            map.insert(key, v);
        }
        // Plus some entirely independent keys.
        for _ in 0..500 {
            let key: Vec<u8> = (0..(1 + (next() % 30) as usize)).map(|_| next()).collect();
            let v = MptValue::Some(Box::from([next()]));
            inc.insert(&key, v.clone());
            map.insert(key, v);
        }
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
