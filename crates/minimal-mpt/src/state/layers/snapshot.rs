use crate::nibbles::{pack_nibbles, NibbleRef, Nibbles};
use crate::trie::{IncrementalTrie, MptValue, TrieValue};
use crate::primitives::H256;
use std::collections::BTreeMap;

/// The snapshot's value type. The snapshot never stores a tombstone (deletes are
/// applied as removals — see [`SnapshotTrie::apply_updates`]), so it drops
/// `MptValue`'s tombstone variant and inlines values up to 8 bytes: the ~61% of
/// snapshot values that are ≤8 B avoid a heap allocation, trading +8 B of inline
/// `BTreeMap` slot for the saved allocation. Merkle bytes are byte-identical to
/// the old `MptValue::Some`, so the snapshot root and checkpoint format are
/// unchanged.
pub(crate) type SnapshotValue = smallvec::SmallVec<u8, 8>;

impl crate::trie::TrieValue for SnapshotValue {
    fn merkle_value(&self) -> &[u8] {
        self.as_slice()
    }
    fn visible_value(&self) -> Option<&[u8]> {
        Some(self.as_slice())
    }
    fn from_value_bytes(bytes: &[u8]) -> Self {
        let mut v = smallvec::SmallVec::new();
        v.extend_from_slice(bytes);
        v
    }
}

#[derive(Clone, Debug)]
pub(crate) struct SnapshotTrie {
    inner: IncrementalTrie<SnapshotValue>,
}

/// The snapshot trie is always built with the `SkipSingleton` cache policy (its
/// millions of singleton subtrees are rarely reused). Every constructor —
/// including this `Default` — pins that policy; the delta's `Full` default must
/// never leak in here.
impl Default for SnapshotTrie {
    fn default() -> Self {
        Self {
            inner: IncrementalTrie::empty_snapshot(),
        }
    }
}

impl SnapshotTrie {
    pub(crate) fn from_snapshot(snapshot: &BTreeMap<Vec<u8>, Box<[u8]>>) -> Self {
        Self {
            inner: IncrementalTrie::from_snapshot(snapshot),
        }
    }

    /// Bulk-feed an **ascending** batch of `(canonical key, value bytes)` whose
    /// keys are all greater than any already present (dense level-2 build).
    /// Values are converted to the snapshot's inline type. Pure memory: the
    /// checkpoint loader in [`crate::persist::stream`] reads batches off disk and
    /// calls this — the snapshot layer never touches a reader.
    pub(crate) fn bulk_extend_sorted(&mut self, batch: Vec<(Vec<u8>, Vec<u8>)>) {
        let converted: Vec<(Vec<u8>, SnapshotValue)> = batch
            .into_iter()
            .map(|(k, v)| {
                (
                    k,
                    <SnapshotValue as crate::trie::TrieValue>::from_value_bytes(&v),
                )
            })
            .collect();
        self.inner.bulk_extend_sorted(converted);
    }

    pub(crate) fn insert(&mut self, key: &[u8], value: MptValue) {
        // The snapshot is tombstone-free: a delete is a removal, never a stored
        // tombstone. Convert the (always-`Some`) value into the inline type.
        match value {
            MptValue::Some(bytes) => self
                .inner
                .insert(key, <SnapshotValue as crate::trie::TrieValue>::from_value_bytes(&bytes)),
            MptValue::Tombstone => self.inner.remove(key),
        }
    }

    pub(crate) fn remove(&mut self, key: &[u8]) {
        self.inner.remove(key);
    }

    pub(crate) fn apply_updates(&mut self, updates: Vec<(Vec<u8>, MptValue)>) {
        for (key, value) in updates {
            match value {
                MptValue::Some(value) => self.insert(&key, MptValue::Some(value)),
                MptValue::Tombstone => self.remove(&key),
            }
        }
    }

    /// Incremental snapshot root. The `SkipSingleton` policy is fixed at
    /// construction (see [`SnapshotTrie::default`]), so this takes no argument.
    pub(crate) fn root(&mut self) -> H256 {
        self.inner.root()
    }

    // --- Value-typed reads: operate directly on the store. These live on the
    // snapshot layer, not on the (value-agnostic) trie / engine. ---

    /// Point lookup by canonical byte key. Returns the live value bytes owned, or
    /// `None` if absent.
    pub(crate) fn snapshot_get_owned(&self, canonical: &[u8]) -> Option<Box<[u8]>> {
        self.inner
            .store()
            .get(canonical)
            .and_then(|v| v.visible_value())
            .map(Box::from)
    }

    /// All `(canonical key, value)` pairs whose canonical key starts with
    /// `canonical_prefix`.
    pub(crate) fn snapshot_scan_prefix(
        &self,
        canonical_prefix: &[u8],
    ) -> Vec<(Vec<u8>, Box<[u8]>)> {
        let buf = pack_nibbles(Nibbles::from_bytes(canonical_prefix));
        self.inner
            .store()
            .prefix_range(NibbleRef::new(&buf))
            .filter_map(|(n, value)| {
                value
                    .visible_value()
                    .map(|v| (n.to_canonical_bytes(), Box::from(v)))
            })
            .collect()
    }

    /// Flatten to a canonical byte-keyed map (for checkpoint persistence).
    pub(crate) fn to_canonical_map(&self) -> BTreeMap<Vec<u8>, Box<[u8]>> {
        self.inner
            .store()
            .iter()
            .filter_map(|(n, value)| {
                value
                    .visible_value()
                    .map(|v| (n.to_canonical_bytes(), Box::from(v)))
            })
            .collect()
    }

    /// Number of live (non-tombstone) entries.
    pub(crate) fn live_entry_count(&self) -> usize {
        self.inner
            .store()
            .iter()
            .filter(|(_, v)| v.visible_value().is_some())
            .count()
    }

    /// Visit each live entry as `(canonical byte key, value bytes)` in sorted
    /// order. Each key is reassembled from its `head ++ tail` on the fly.
    pub(crate) fn for_each_canonical(
        &self,
        mut f: impl FnMut(Vec<u8>, &[u8]) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        for (n, value) in self.inner.store().iter() {
            if let Some(v) = value.visible_value() {
                f(n.to_canonical_bytes(), v)?;
            }
        }
        Ok(())
    }

    pub(crate) fn len(&self) -> usize {
        self.inner.len()
    }
}
