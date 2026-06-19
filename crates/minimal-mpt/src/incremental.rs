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

use crate::trie::{
    bytes_to_nibbles, compute_node_merkle, compute_path_merkle, MptValue, CHILDREN,
};
use crate::types::{H256, MERKLE_NULL_NODE};
use rustc_hash::FxHashMap;
use std::collections::{BTreeMap, BTreeSet};

/// Cache keyed by a node's routing nibble prefix. FxHash, not SipHash: these
/// short byte (nibble) keys are hashed on every node visit and SipHash showed
/// up prominently in profiles.
type Cache = FxHashMap<Vec<u8>, H256>;

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

        let entries: Vec<Entry> =
            self.entries.iter().map(|(k, v)| (k.as_slice(), v)).collect();
        memo_node(&entries, 0, false, &mut self.cache)
    }
}

/// `(nibble key, value)` references into the persistent entry map.
type Entry<'a> = (&'a [u8], &'a MptValue);

/// Mirror of [`crate::trie`]'s `merkle_for_node`, plus a cache keyed by the
/// node's routing prefix `entries[0].0[0..depth]`. A hit is always valid because
/// `root` evicted any prefix touched by a dirty key. Serial only (the cache is
/// `&mut`); the incremental path visits few nodes so the stateless version's
/// rayon parallelism is not needed here.
fn memo_node(
    entries: &[Entry],
    depth: usize,
    allow_path_compression: bool,
    cache: &mut Cache,
) -> H256 {
    let prefix = &entries[0].0[0..depth];
    if let Some(hash) = cache.get(prefix) {
        return *hash;
    }

    // --- structure identical to trie::merkle_for_node ---
    let common = if allow_path_compression {
        common_prefix_len(entries, depth)
    } else {
        0
    };
    let node_depth = depth + common;

    let maybe_value = entries
        .iter()
        .find(|(nibbles, _)| nibbles.len() == node_depth)
        .map(|(_, value)| value.merkle_value());

    let mut ranges: Vec<(usize, usize, usize)> = Vec::new();
    let mut i = 0;
    while i < entries.len() {
        if entries[i].0.len() == node_depth {
            i += 1;
            continue;
        }
        let child = entries[i].0[node_depth] as usize;
        let start = i;
        i += 1;
        while i < entries.len()
            && entries[i].0.len() > node_depth
            && entries[i].0[node_depth] as usize == child
        {
            i += 1;
        }
        ranges.push((child, start, i));
    }

    let mut children: [H256; CHILDREN] = [MERKLE_NULL_NODE; CHILDREN];
    for (child, start, end) in ranges {
        children[child] =
            memo_node(&entries[start..end], node_depth + 1, true, cache);
    }

    let has_children = children.iter().any(|h| *h != MERKLE_NULL_NODE);
    let node_hash = compute_node_merkle(has_children.then_some(&children), maybe_value);
    let hash = compute_path_merkle(&entries[0].0[depth..node_depth], depth, node_hash);

    cache.insert(entries[0].0[0..depth].to_vec(), hash);
    hash
}

/// Mirror of `trie::common_prefix_len` over the `(nibbles, value)` tuple.
fn common_prefix_len(entries: &[Entry], depth: usize) -> usize {
    let first = &entries[0].0;
    let mut len = 0;
    'outer: loop {
        let idx = depth + len;
        if idx >= first.len() {
            break;
        }
        let nibble = first[idx];
        for (key, _) in entries.iter().skip(1) {
            if idx >= key.len() || key[idx] != nibble {
                break 'outer;
            }
        }
        len += 1;
    }
    len
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
                map.insert(key.clone(), val(v as u8));
                inc.insert(&key, val(v as u8));
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
                let key: Vec<u8> =
                    (0..40).map(|_| (lcg(&mut seed) & 0xff) as u8).collect();
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
