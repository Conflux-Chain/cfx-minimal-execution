use crate::nibbles::{pack_nibbles, NibbleCursor, PrefixRef};
use crate::primitives::H256;
use crate::trie::store::two_level::TwoLevelStore;
use crate::trie::{snapshot_value, AuthenticatedTrie, MptValue};
use std::collections::BTreeMap;

/// The snapshot layer: an [`AuthenticatedTrie`] over the two-level
/// [`TwoLevelStore`], serving snapshot reads and the incremental snapshot root.
/// The snapshot is **tombstone-free** by construction — a delete is applied as a
/// removal (see [`SnapshotTrie::insert`]) — so every stored entry is a live
/// value (the store's inline `SnapshotValue`; see [`crate::trie`]).
///
/// `pub` only for the `incremental_trie` differential fuzz (re-exported under
/// `#[cfg(fuzzing)]` in `lib.rs`, alongside [`super::delta::DeltaTrie`]); the
/// enclosing modules are crate-private, so nothing leaks from a normal build.
#[derive(Clone, Debug)]
pub struct SnapshotTrie {
    inner: AuthenticatedTrie<TwoLevelStore>,
}

/// Empty snapshot: `SkipSingleton` cache policy. NOT derived — a derived
/// `Default` would pick up `AuthenticatedTrie`'s delta default (`Full` policy).
impl Default for SnapshotTrie {
    fn default() -> Self {
        Self {
            inner: AuthenticatedTrie::empty_snapshot(),
        }
    }
}

impl SnapshotTrie {
    pub(crate) fn from_snapshot(snapshot: &BTreeMap<Vec<u8>, Box<[u8]>>) -> Self {
        Self {
            inner: AuthenticatedTrie::from_snapshot(snapshot),
        }
    }

    /// Bulk-feed an **ascending** batch of `(canonical key, value bytes)` whose
    /// keys are all greater than any already present (dense level-2 build).
    /// Pure memory: the checkpoint loader in [`crate::persist::stream`] reads
    /// batches off disk and calls this — the snapshot layer never touches a
    /// reader.
    pub(crate) fn bulk_extend_sorted(&mut self, batch: Vec<(Vec<u8>, Vec<u8>)>) {
        self.inner.bulk_extend_sorted(
            batch
                .into_iter()
                .map(|(k, v)| (k, snapshot_value(&v)))
                .collect(),
        );
    }

    /// Apply a delta value: a live value is stored, a tombstone is a removal —
    /// the snapshot never stores the tombstone itself.
    pub fn insert(&mut self, key: &[u8], value: MptValue) {
        match value {
            MptValue::Some(bytes) => self.inner.insert(key, snapshot_value(&bytes)),
            MptValue::Tombstone => self.inner.remove(key),
        }
    }

    pub(crate) fn apply_updates(&mut self, updates: Vec<(Vec<u8>, MptValue)>) {
        for (key, value) in updates {
            self.insert(&key, value);
        }
    }

    /// Incremental snapshot root. The `SkipSingleton` policy is fixed at
    /// construction (see [`SnapshotTrie::default`]), so this takes no argument.
    pub fn root(&mut self) -> H256 {
        self.inner.root()
    }

    // --- Value-typed reads: operate directly on the store. These live on the
    // snapshot layer, not on the (value-agnostic) trie / engine. ---

    /// Point lookup by canonical byte key. Returns the value bytes owned, or
    /// `None` if absent.
    pub(crate) fn snapshot_get_owned(&self, canonical: &[u8]) -> Option<Box<[u8]>> {
        self.inner
            .store()
            .get(canonical)
            .map(|v| Box::from(v.as_slice()))
    }

    /// All `(canonical key, value)` pairs whose canonical key starts with
    /// `canonical_prefix`.
    pub(crate) fn snapshot_scan_prefix(
        &self,
        canonical_prefix: &[u8],
    ) -> Vec<(Vec<u8>, Box<[u8]>)> {
        let buf = pack_nibbles(NibbleCursor::from_bytes(canonical_prefix));
        self.inner
            .store()
            .prefix_range(PrefixRef::new(&buf))
            .map(|(n, value)| (n.to_canonical_bytes(), Box::from(value.as_slice())))
            .collect()
    }

    /// Flatten to a canonical byte-keyed map (for checkpoint persistence).
    pub(crate) fn to_canonical_map(&self) -> BTreeMap<Vec<u8>, Box<[u8]>> {
        self.inner
            .store()
            .iter()
            .map(|(n, value)| (n.to_canonical_bytes(), Box::from(value.as_slice())))
            .collect()
    }

    /// Number of entries (all live — the snapshot holds no tombstones).
    pub(crate) fn live_entry_count(&self) -> usize {
        self.inner.len()
    }

    /// Visit each entry as `(canonical byte key, value bytes)` in sorted order.
    /// Each key is reassembled from its `head ++ tail` on the fly.
    pub(crate) fn for_each_canonical(
        &self,
        mut f: impl FnMut(Vec<u8>, &[u8]) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        for (n, value) in self.inner.store().iter() {
            f(n.to_canonical_bytes(), value)?;
        }
        Ok(())
    }

    pub(crate) fn len(&self) -> usize {
        self.inner.len()
    }
}
