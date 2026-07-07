//! The intermediate layer.
//!
//! A FROZEN [`FlatStore`] plus the delta root it carried when it rotated down out
//! of the delta position. Within a period the intermediate is read-only and never
//! recomputes its root: the value is the delta root remembered from the last
//! commit before rotation (or, at checkpoint load, computed once via the stateless
//! oracle). Its key space is the same delta-mpt-padded space as the delta it
//! descended from — the same representation, so its prefix scans and point reads
//! behave exactly as the delta's do.

use crate::primitives::{H256, MERKLE_NULL_NODE};
use crate::trie::reader::TrieStore;
use crate::trie::store::flat::FlatStore;
use crate::trie::MptValue;
use std::collections::BTreeMap;

#[derive(Clone, Debug)]
pub(crate) struct IntermediateLayer {
    store: FlatStore<MptValue>,
    /// Remembered root — never recomputed (see module docs).
    root: H256,
}

/// Genesis intermediate: empty store, `MERKLE_NULL_NODE` root (NOT `H256::zero`,
/// so a derived `Default` would be wrong here).
impl Default for IntermediateLayer {
    fn default() -> Self {
        Self {
            store: FlatStore::new(),
            root: MERKLE_NULL_NODE,
        }
    }
}

impl IntermediateLayer {
    /// Build from a persisted byte-keyed intermediate and its root (computed once
    /// via the stateless oracle by the caller — checkpoint load path).
    pub(crate) fn from_map(map: &BTreeMap<Vec<u8>, MptValue>, root: H256) -> Self {
        let mut store = FlatStore::new();
        for (k, v) in map {
            store.insert(k, v.clone());
        }
        Self { store, root }
    }

    /// Adopt a just-finished delta's store as-is and remember its delta root as
    /// this layer's frozen root (rotation path — no re-materialization, no
    /// re-hash).
    pub(crate) fn from_store(store: FlatStore<MptValue>, root: H256) -> Self {
        Self { store, root }
    }

    /// Point lookup by exact (delta-mpt-padded) byte key.
    pub(crate) fn get(&self, key: &[u8]) -> Option<&MptValue> {
        self.store.get(key)
    }

    /// The remembered intermediate-delta root.
    pub(crate) fn root(&self) -> H256 {
        self.root
    }

    /// Number of stored entries (tombstones included).
    pub(crate) fn len(&self) -> usize {
        self.store.len()
    }

    /// Ascending byte-prefix scan over the raw (delta-mpt-padded) keys.
    pub(crate) fn scan_prefix<'a>(
        &'a self,
        prefix: &'a [u8],
    ) -> impl Iterator<Item = (&'a [u8], &'a MptValue)> {
        self.store.scan_byte_prefix(prefix)
    }

    /// The inner store map for direct (byte-identical) checkpoint serialization.
    pub(crate) fn raw_entries(&self) -> &BTreeMap<Box<[u8]>, MptValue> {
        self.store.raw_entries()
    }

    /// A `BTreeMap<Vec<u8>, MptValue>` copy (checkpoint `PersistedState` image).
    pub(crate) fn to_btreemap(&self) -> BTreeMap<Vec<u8>, MptValue> {
        self.store.to_value_map()
    }

    /// Consume the layer, yielding each `(raw delta-mpt key, value)` in ascending
    /// key order — the snapshot-absorb pass at a period boundary.
    pub(crate) fn into_entries(self) -> impl Iterator<Item = (Box<[u8]>, MptValue)> {
        self.store.into_entries().into_iter()
    }
}
