//! Cold-cache parallel root/cache rebuild.
//!
//! When the cache is empty and the store is large, [`memo_node_rebuild`] rebuilds
//! the whole root (and cache) from a single sorted entry slice, fanning out
//! across cores by subtree. Extracted verbatim from the old `incremental` module;
//! the partitioning and hashing are identical to the serial [`super::memo`] path.

use super::engine::{Cache, CachePolicy};
use super::memo::memo_node_via;
use super::{PARALLEL_REBUILD_THRESHOLD, SHARD_CAP};
use crate::nibbles::{pack_nibbles, push_nibble, NibbleCursor, PrefixKey, PrefixRef};
use crate::primitives::{H256, MERKLE_NULL_NODE};
use crate::trie::reader::TrieReader;
use crate::trie::{compute_node_merkle, compute_path_merkle, CHILDREN};
use rayon::prelude::*;

/// Cold-cache rebuild of a whole subtree, bounded to [`SHARD_CAP`] transient
/// entries: a subtree that fits is materialized once and handed to the parallel
/// [`memo_node_rebuild`] (its cache entries go straight into `cache` and both
/// buffers die before the next shard starts); a bigger subtree is dispatched
/// child-by-child through the ordinary [`memo_node_via`] walk, recursing back
/// here. So the peak holds ONE shard's entry slice (~192 MiB) instead of the
/// whole store's (~11 GiB at checkpoint scale), and the cache-entry vec is
/// per-shard instead of store-wide.
///
/// The dispatch nodes above the shards are exactly the nodes `memo_node` would
/// visit, with identical hashing/caching (the walk body is shared, not
/// mirrored); a probe of `cap + 1` entries decides fits-vs-dispatch, so the
/// wasted iteration is bounded by `cap × dispatch nodes` (a handful of nodes).
pub(super) fn memo_node_sharded<S: TrieReader>(
    store: &S,
    rp: PrefixRef,
    allow_path_compression: bool,
    cache: &mut Cache,
    cache_policy: CachePolicy,
) -> H256 {
    memo_node_sharded_capped(
        store,
        rp,
        allow_path_compression,
        cache,
        cache_policy,
        SHARD_CAP,
    )
}

fn memo_node_sharded_capped<S: TrieReader>(
    store: &S,
    rp: PrefixRef,
    allow_path_compression: bool,
    cache: &mut Cache,
    cache_policy: CachePolicy,
    cap: usize,
) -> H256 {
    let entries: Vec<EntryRef<'_>> = store
        .iter_merkle_prefix(rp)
        .take(cap.saturating_add(1))
        .map(|(key, value)| {
            // The full key as its two bare segments (48 B/entry; see
            // `EntryRef`) — the store always surfaces full-key views.
            let (key_head, key_tail) = key
                .full_view_segments()
                .expect("store surfaces full-key views");
            EntryRef {
                key_head,
                key_tail,
                value,
            }
        })
        .collect();
    debug_assert!(!entries.is_empty(), "descended into an empty subtree");

    if entries.len() <= cap {
        let (hash, cache_entries) =
            memo_node_rebuild(&entries, rp, allow_path_compression, cache_policy);
        drop(entries);
        cache.reserve(cache_entries.len());
        cache.extend(cache_entries);
        return hash;
    }
    drop(entries);

    memo_node_via(
        store,
        rp,
        allow_path_compression,
        cache,
        cache_policy,
        &|s, child, c| memo_node_sharded_capped(s, child, true, c, cache_policy, cap),
    )
}

/// One entry fed to the cold [`memo_node_rebuild`]: the full key as its two
/// backing byte segments (from [`NibbleCursor::full_view_segments`]) plus its
/// precomputed **merkle bytes** (empty for a tombstone). The rebuild only ever
/// needs the merkle bytes, so the store's
/// [`TrieReader`](crate::trie::reader::TrieReader) impl flattens its value type
/// up front — keeping the rebuild independent of it. Nibble routing rebuilds
/// the [`NibbleCursor`] view on demand ([`Self::nibbles`], zero cost): shards of
/// up to [`SHARD_CAP`] of these are materialized back-to-back during checkpoint
/// recovery, so the entry stays three bare slices (48 B) — a stored cursor's
/// redundant offsets would inflate every shard slice by half.
#[derive(Clone, Copy)]
pub(super) struct EntryRef<'a> {
    pub(super) key_head: &'a [u8],
    pub(super) key_tail: &'a [u8],
    pub(super) value: &'a [u8],
}

impl<'a> EntryRef<'a> {
    #[inline]
    fn nibbles(&self) -> NibbleCursor<'a> {
        NibbleCursor::from_composite(self.key_head, self.key_tail)
    }
    #[inline]
    fn nibble_len(&self) -> usize {
        (self.key_head.len() + self.key_tail.len()) * 2
    }
}

/// Cold-cache root/cache rebuild over an already sorted entry slice.
///
/// This mirrors `memo_node`, but it uses contiguous slice partitions instead of
/// BTreeMap range probes. Because the caller only uses it when the cache is
/// empty, children return cache entries as vectors; the caller builds the real
/// hash map once after all parallel work joins.
pub(super) fn memo_node_rebuild(
    entries: &[EntryRef<'_>],
    rp: PrefixRef,
    allow_path_compression: bool,
    cache_policy: CachePolicy,
) -> (H256, Vec<(PrefixKey, H256)>) {
    debug_assert!(!entries.is_empty());

    let depth = rp.nibble_len();
    let first = entries[0].nibbles();
    let last = entries[entries.len() - 1].nibbles();
    let is_singleton = first.logical_eq(&last);
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
        let (hash, cache_entries) = memo_node_rebuild(
            &entries[start..end],
            PrefixRef::new(&child_buf),
            true,
            cache_policy,
        );
        (child, hash, cache_entries)
    };

    // Fan out across cores only for a large, multi-child block; recursion turns
    // serial once a subtree drops below the threshold, bounding task
    // granularity. Splitting by SUBTREE SIZE (not a fixed depth) lets a few
    // oversized subtrees — e.g. a large-storage contract — spread across cores.
    // Nested par_iter is fine: rayon work-steals over the shared pool.
    let ranges = child_ranges(entries, node_depth);
    let results: Vec<(usize, H256, Vec<(PrefixKey, H256)>)> =
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
        cache_entries.push((PrefixKey::from_bytes(rp.as_bytes()), hash));
    }
    (hash, cache_entries)
}

#[cfg(test)]
mod tests {
    use super::super::memo::memo_node;
    use super::*;
    use crate::trie::reader::TrieStore;
    use crate::trie::store::btree::BTreeStore;
    use crate::trie::store::two_level::TwoLevelStore;
    use crate::trie::{snapshot_value, MptValue};

    /// For every shard cap — from "dispatch all the way to singletons" (1)
    /// through "one shot, no dispatch" (MAX) — the sharded rebuild must produce
    /// the same root AND the byte-identical cache as the serial `memo_node`
    /// walk, under both cache policies.
    fn assert_sharded_matches_memo<S: TrieReader>(store: &S) {
        for policy in [CachePolicy::Full, CachePolicy::SkipSingleton] {
            let mut want_cache = Cache::default();
            let want_root = memo_node(store, PrefixRef::empty(), false, &mut want_cache, policy);
            for cap in [1, 7, 313, usize::MAX] {
                let mut cache = Cache::default();
                let root = memo_node_sharded_capped(
                    store,
                    PrefixRef::empty(),
                    false,
                    &mut cache,
                    policy,
                    cap,
                );
                assert_eq!(
                    root, want_root,
                    "root diverged at cap={cap} policy={policy:?}"
                );
                assert_eq!(
                    cache, want_cache,
                    "cache diverged at cap={cap} policy={policy:?}"
                );
            }
        }
    }

    /// Flat store: short keys with shared prefixes, tombstones and empty-`Some`
    /// values interleaved (the terminal-value / tombstone-collapse edge cases).
    #[test]
    fn sharded_matches_memo_flat() {
        let mut store = BTreeStore::new();
        for i in 0u32..3000 {
            let key = vec![(i & 0xff) as u8, (i >> 8) as u8, ((i >> 3) & 0xff) as u8];
            let v = match i % 5 {
                0 => MptValue::Tombstone,
                1 => MptValue::Some(Box::from([])),
                _ => MptValue::Some(Box::from([(i as u8) | 1])),
            };
            TrieStore::insert(&mut store, &key, v);
        }
        // A key ending exactly at an interior dispatch node (terminal value).
        TrieStore::insert(&mut store, &[], MptValue::Some(Box::from([9])));
        TrieStore::insert(&mut store, &[7], MptValue::Some(Box::from([9])));
        assert_sharded_matches_memo(&store);
    }

    /// Two-level store: realistic 57-byte keys with a whale group sharing a
    /// >23-byte prefix, so small caps force the dispatch to descend past the
    /// level-1 head (deep `iter_merkle_prefix` ranges) and through the whale's
    /// compressed path in one common-prefix jump.
    #[test]
    fn sharded_matches_memo_two_level() {
        let mut seed: u64 = 0xabcdef12345678;
        let mut next = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            (seed >> 24) as u8
        };
        let mut store = TwoLevelStore::new();
        let whale: Vec<u8> = (0..20).map(|_| next()).collect();
        for _ in 0..2000 {
            let mut key = whale.clone();
            key.extend((0..37).map(|_| next()));
            store.insert(&key, snapshot_value(&[next(), next()]));
        }
        for _ in 0..500 {
            let key: Vec<u8> = (0..(1 + (next() % 30) as usize)).map(|_| next()).collect();
            store.insert(&key, snapshot_value(&[next()]));
        }
        assert_sharded_matches_memo(&store);
    }
}

/// Partition a sorted node block into `(child nibble, start, end)` runs, one per
/// child subtree. TwoLevelStore whose nibble length equals `node_depth` carry no child
/// nibble and are dropped; the rest — being sorted — group into contiguous runs
/// sharing their `node_depth`-th nibble.
fn child_ranges(entries: &[EntryRef<'_>], node_depth: usize) -> Vec<(usize, usize, usize)> {
    entries
        .iter()
        .enumerate()
        .filter_map(|(idx, entry)| {
            (node_depth < entry.nibble_len())
                .then(|| (idx, entry.nibbles().at(node_depth) as usize))
        })
        .fold(Vec::new(), |mut ranges, (idx, child)| {
            match ranges.last_mut() {
                Some(last) if last.0 == child && last.2 == idx => last.2 = idx + 1,
                _ => ranges.push((child, idx, idx + 1)),
            }
            ranges
        })
}
