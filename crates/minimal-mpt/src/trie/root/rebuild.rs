//! Cold-cache parallel root/cache rebuild.
//!
//! When the cache is empty and the store is large, [`memo_node_rebuild`] rebuilds
//! the whole root (and cache) from a single sorted entry slice, fanning out
//! across cores by subtree. Extracted verbatim from the old `incremental` module;
//! the partitioning and hashing are identical to the serial [`super::memo`] path.

use super::engine::CachePolicy;
use super::PARALLEL_REBUILD_THRESHOLD;
use crate::nibbles::{pack_nibbles, push_nibble, NibbleCursor, PrefixKey, PrefixRef};
use crate::primitives::{H256, MERKLE_NULL_NODE};
use crate::trie::{compute_node_merkle, compute_path_merkle, CHILDREN};
use rayon::prelude::*;

/// One entry fed to the cold [`memo_node_rebuild`]: the full key as its two
/// backing byte segments (from [`NibbleCursor::full_view_segments`]) plus its
/// precomputed **merkle bytes** (empty for a tombstone). The rebuild only ever
/// needs the merkle bytes, so the store's
/// [`TrieReader`](crate::trie::reader::TrieReader) impl flattens its value type
/// up front — keeping the rebuild independent of it. Nibble routing rebuilds
/// the [`NibbleCursor`] view on demand ([`Self::nibbles`], zero cost): this array is
/// materialized for the WHOLE store (hundreds of millions of entries) during
/// checkpoint recovery, so the entry stays three bare slices (48 B) — a stored
/// cursor's redundant offsets would put ~6 GiB back on the load peak.
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
