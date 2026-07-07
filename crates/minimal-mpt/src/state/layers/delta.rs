//! The delta layer.
//!
//! The in-flight period's writes live here as a single
//! `AuthenticatedTrie<FlatStore<MptValue>>` (`Full` cache policy). This is the one
//! source of truth for the delta: the point-read / prefix store and the
//! incremental delta root are the *same* structure, so there is no second copy.
//! It replaces the historical pair of `State.delta` (a byte-keyed `BTreeMap`) and
//! `delta_inc` (a two-level `IncrementalTrie` holding a redundant copy of the same
//! entries alongside the cached subtree hashes) — collapsing P1's double storage.
//!
//! Keys are the delta-mpt-padded canonical keys, exactly as the old `State.delta`
//! stored them; the delta root is computed over those same bytes with the same
//! `Full` policy, so it is byte-identical to the old `delta_inc` root.

use crate::primitives::H256;
use crate::trie::reader::TrieStore;
use crate::trie::store::flat::FlatStore;
use crate::trie::{AuthenticatedTrie, MptValue};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default)]
pub(crate) struct DeltaLayer {
    trie: AuthenticatedTrie<FlatStore<MptValue>>,
}

impl DeltaLayer {
    /// Build a delta layer from a persisted byte-keyed delta (checkpoint load).
    /// The store is populated directly (no per-key dirty marking); the first
    /// `root` does a full recompute that warms the cache.
    pub(crate) fn from_map(map: &BTreeMap<Vec<u8>, MptValue>) -> Self {
        let mut store = FlatStore::new();
        for (k, v) in map {
            store.insert(k, v.clone());
        }
        Self {
            trie: AuthenticatedTrie::from_store(store),
        }
    }

    /// Insert / overwrite `key` (delta-mpt-padded), marking the subtree dirty.
    pub(crate) fn insert(&mut self, key: &[u8], value: MptValue) {
        self.trie.insert(key, value);
    }

    /// Remove `key`, marking the subtree dirty.
    pub(crate) fn remove(&mut self, key: &[u8]) {
        self.trie.remove(key);
    }

    /// Point lookup by exact (delta-mpt-padded) byte key.
    pub(crate) fn get(&self, key: &[u8]) -> Option<&MptValue> {
        self.trie.store().get(key)
    }

    /// Incremental delta root (reuses cached hashes for unchanged subtrees).
    pub(crate) fn root(&mut self) -> H256 {
        self.trie.root()
    }

    /// Number of stored entries (tombstones included).
    pub(crate) fn len(&self) -> usize {
        self.trie.len()
    }

    /// Ascending byte-prefix scan over the raw (delta-mpt-padded) keys.
    pub(crate) fn scan_prefix<'a>(
        &'a self,
        prefix: &'a [u8],
    ) -> impl Iterator<Item = (&'a [u8], &'a MptValue)> {
        self.trie.store().scan_byte_prefix(prefix)
    }

    /// The inner store map for direct (byte-identical) checkpoint serialization.
    pub(crate) fn raw_entries(&self) -> &BTreeMap<Box<[u8]>, MptValue> {
        self.trie.store().raw_entries()
    }

    /// A `BTreeMap<Vec<u8>, MptValue>` copy (checkpoint `PersistedState` image and
    /// the `verify-incremental` oracle cross-check).
    pub(crate) fn to_btreemap(&self) -> BTreeMap<Vec<u8>, MptValue> {
        self.trie.store().to_value_map()
    }

    /// Consume the layer, yielding its store — MOVED into the intermediate layer
    /// at a period boundary with no re-materialization (the cache/dirty engine is
    /// dropped; the intermediate remembers the delta root instead).
    pub(crate) fn into_store(self) -> FlatStore<MptValue> {
        self.trie.into_store()
    }
}
