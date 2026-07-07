//! The incremental root engine: subtree-hash cache + pending dirty set + prefix
//! eviction + the root walk, with the [`CachePolicy`] **fixed at construction**.
//!
//! [`RootEngine`] owns everything the incremental root needs *except* the entries
//! themselves: the walk reaches those through a borrowed [`TrieReader`] passed to
//! [`RootEngine::root`]. It never names the stored value type `V` — it sees only
//! merkle bytes surfaced by the reader — so one engine serves every store /
//! value type. The policy (delta = [`CachePolicy::Full`], snapshot =
//! [`CachePolicy::SkipSingleton`]) is chosen once by the owning layer and never
//! varies per call, replacing the old per-call `root_with_policy` argument.

use super::memo::memo_node;
use super::rebuild::{memo_node_rebuild, EntryRef};
use super::PARALLEL_REBUILD_THRESHOLD;
use crate::nibbles::{empty_prefix_buf, push_nibble, NibblePrefix, NibbleRef};
use crate::primitives::{H256, MERKLE_NULL_NODE};
use crate::trie::reader::TrieReader;
use rustc_hash::FxHashMap;
use std::collections::BTreeSet;

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

/// Cache + dirty tracking + prefix eviction for the incremental trie root, with
/// the [`CachePolicy`] pinned at construction. Holds no entries and no value
/// type: [`RootEngine::root`] borrows the store as a [`TrieReader`].
///
/// A commit costs roughly `O(changed keys × depth)` instead of `O(entry count)`:
/// writers call [`RootEngine::mark_dirty`] with each changed canonical key
/// (cheap, no hashing), and the recompute is batched to the next
/// [`RootEngine::root`], which evicts every prefix of every dirty key before
/// walking so no stale subtree hash can survive.
#[derive(Clone, Debug)]
pub(crate) struct RootEngine {
    /// Cached subtree hashes, keyed by node routing prefix.
    cache: Cache,
    /// Canonical keys changed since the last `root`, pending prefix eviction.
    dirty: BTreeSet<Box<[u8]>>,
    /// Which subtree hashes are retained; fixed for this engine's lifetime.
    policy: CachePolicy,
}

impl RootEngine {
    /// A fresh engine with the given retention policy and empty cache/dirty set.
    pub(crate) fn new(policy: CachePolicy) -> Self {
        Self {
            cache: Cache::default(),
            dirty: BTreeSet::new(),
            policy,
        }
    }

    /// Record that `key` (canonical byte mpt key) changed, so the next `root`
    /// evicts its subtree hashes. Cheap: no hashing, no store access.
    pub(crate) fn mark_dirty(&mut self, key: &[u8]) {
        self.dirty.insert(Box::from(key));
    }

    /// Drop all cached hashes and pending dirty keys (e.g. on a delta reset).
    // Reached only through `AuthenticatedTrie::clear`, whose sole caller is the
    // `empty_after_clear_is_null` test — reads dead in the lib build alone.
    #[allow(dead_code)]
    pub(crate) fn clear(&mut self) {
        self.cache.clear();
        self.dirty.clear();
    }

    /// Merkle root of `store`, reusing cached hashes for unchanged subtrees.
    /// Clears the pending dirty set; keeps the cache. Uses the engine's fixed
    /// [`CachePolicy`].
    pub(crate) fn root(&mut self, store: &impl TrieReader) -> H256 {
        let cache_policy = self.policy;
        if store.is_empty() {
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

        let result = if self.cache.is_empty() && store.len() >= PARALLEL_REBUILD_THRESHOLD {
            let timing = crate::diag::recover_timing();
            let entries: Vec<EntryRef<'_>> = store
                .iter_merkle()
                .map(|(key, value)| EntryRef {
                    key,   // Nibbles view surfaced by the store
                    value, // precomputed merkle bytes (value type stays behind the trait)
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
                store,
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
            // canonical keys. It hashes only `merkle_value()` (tombstone and
            // empty-`Some` both collapse to `&[]`), so wrapping the store's
            // surfaced merkle bytes as `Some(..)` reproduces the identical oracle
            // root — the value type stays entirely behind the trait here too.
            let oracle_owned: Vec<(Vec<u8>, crate::trie::MptValue)> = store
                .iter_merkle()
                .map(|(n, merkle)| {
                    let nibbles = crate::trie::bytes_to_nibbles(&n.to_canonical_bytes());
                    (nibbles, crate::trie::MptValue::Some(Box::from(merkle)))
                })
                .collect();
            let oracle: Vec<(Vec<u8>, &crate::trie::MptValue)> =
                oracle_owned.iter().map(|(k, v)| (k.clone(), v)).collect();
            assert_eq!(
                result,
                crate::trie::root_for_entries(&oracle),
                "incremental root diverged from stateless trie_root over identical entries"
            );
        }
        result
    }
}
