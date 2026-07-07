use crate::{incremental::CachePolicy, trie::MptValue, types::H256};
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

#[derive(Clone, Debug, Default)]
pub(crate) struct SnapshotTrie {
    inner: crate::incremental::IncrementalTrie<SnapshotValue>,
}

impl SnapshotTrie {
    pub(crate) fn from_snapshot(snapshot: &BTreeMap<Vec<u8>, Box<[u8]>>) -> Self {
        Self {
            inner: crate::incremental::IncrementalTrie::from_snapshot(snapshot),
        }
    }

    /// Stream the snapshot trie from a checkpoint reader, converting keys to
    /// nibbles on the fly — no byte-keyed `BTreeMap` is ever built.
    pub(crate) fn from_reader(reader: &mut impl std::io::Read) -> std::io::Result<Self> {
        Ok(Self {
            inner: crate::incremental::IncrementalTrie::from_snapshot_reader(reader)?,
        })
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

    pub(crate) fn root_with_policy(&mut self, cache_policy: CachePolicy) -> H256 {
        self.inner.root_with_policy(cache_policy)
    }

    pub(crate) fn snapshot_get_owned(&self, canonical: &[u8]) -> Option<Box<[u8]>> {
        self.inner.snapshot_get(canonical).map(Box::from)
    }

    pub(crate) fn snapshot_scan_prefix(
        &self,
        canonical_prefix: &[u8],
    ) -> Vec<(Vec<u8>, Box<[u8]>)> {
        self.inner.snapshot_scan_prefix(canonical_prefix)
    }

    pub(crate) fn to_canonical_map(&self) -> BTreeMap<Vec<u8>, Box<[u8]>> {
        self.inner.to_canonical_map()
    }

    pub(crate) fn live_entry_count(&self) -> usize {
        self.inner.live_entry_count()
    }

    pub(crate) fn for_each_canonical(
        &self,
        mut f: impl FnMut(Vec<u8>, &[u8]) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        for (key, value) in self.inner.iter_canonical() {
            f(key, value)?;
        }
        Ok(())
    }

    pub(crate) fn len(&self) -> usize {
        self.inner.len()
    }
}
