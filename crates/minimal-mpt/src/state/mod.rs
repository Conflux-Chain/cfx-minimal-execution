use crate::diag::profile;
use crate::state::layers::delta::DeltaTrie;
use crate::state::layers::intermediate::IntermediateTrie;
use crate::state::layers::snapshot::SnapshotTrie;
use crate::{
    keys::{DeltaMptKeyPadding, StorageKeyWithSpace},
    persist::format::PersistedState,
    primitives::{MptKeyValue, H256, MERKLE_NULL_NODE},
    trie::{trie_root, MptValue},
    Result,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

mod commit;
pub(crate) mod layers;
mod prefix;
mod rotation;

pub const DEFAULT_SNAPSHOT_EPOCH_COUNT: u32 = 2000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitRoot {
    pub snapshot_root: H256,
    pub intermediate_delta_root: H256,
    pub delta_root: H256,
    pub state_root_hash: H256,
    pub delta_mpt_key_padding: [u8; 32],
}

impl CommitRoot {
    pub fn new(
        snapshot_root: H256,
        intermediate_delta_root: H256,
        delta_root: H256,
        delta_mpt_key_padding: [u8; 32],
    ) -> Self {
        let mut buf = Vec::with_capacity(96);
        buf.extend_from_slice(snapshot_root.as_bytes());
        buf.extend_from_slice(intermediate_delta_root.as_bytes());
        buf.extend_from_slice(delta_root.as_bytes());
        let state_root_hash = crate::trie::keccak(&buf);
        Self {
            snapshot_root,
            intermediate_delta_root,
            delta_root,
            state_root_hash,
            delta_mpt_key_padding,
        }
    }
}

pub trait StateTrait {
    fn get(&self, key: StorageKeyWithSpace) -> Result<Option<Box<[u8]>>>;
    fn set(&mut self, key: StorageKeyWithSpace, value: Box<[u8]>) -> Result<()>;
    fn get_all_by_prefix(&self, prefix: StorageKeyWithSpace) -> Result<Option<Vec<MptKeyValue>>>;
    fn delete_all_by_prefix(
        &mut self,
        prefix: StorageKeyWithSpace,
    ) -> Result<Option<Vec<MptKeyValue>>>;
    fn commit(&mut self) -> Result<CommitRoot>;
}

#[derive(Debug)]
pub struct State {
    /// The snapshot as a single two-level incremental trie: it serves snapshot reads and
    /// owns the persistent subtree-hash cache, so the boundary merge re-roots it
    /// incrementally **in place**. Read-only during a period; only
    /// `advance_after_commit` mutates it, at boundaries. No clone, no double
    /// buffer: the incremental root is cheap enough to do synchronously.
    snapshot: SnapshotTrie,
    /// The frozen intermediate trie: a `BTreeStore` plus its remembered
    /// intermediate-delta root (see [`IntermediateTrie`]).
    intermediate: IntermediateTrie,
    /// The in-flight delta trie: a single `AuthenticatedTrie<BTreeStore>` that is
    /// *both* the point-read/prefix store and the incremental delta root — one
    /// copy, not the old `BTreeMap` + `delta_inc` pair (see [`DeltaTrie`]).
    delta: DeltaTrie,
    snapshot_root: H256,
    intermediate_padding: DeltaMptKeyPadding,
    delta_padding: DeltaMptKeyPadding,
    height: u64,
    snapshot_epoch_count: u32,
    last_root: Option<CommitRoot>,
    /// Per-period caches of `new_account_key` derivations (the hot keccak in
    /// `to_delta_mpt_key_bytes`), one per active padding. `RefCell` so `get` can
    /// fill them behind `&self`; padding-stamped so they self-invalidate, and
    /// rotated at the boundary — `intermediate(k)`'s padding equals `delta(k-1)`'s,
    /// so the old delta cache is exactly the new intermediate one.
    delta_account_cache: std::cell::RefCell<crate::keys::AccountKeyCache>,
    intermediate_account_cache: std::cell::RefCell<crate::keys::AccountKeyCache>,
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

impl State {
    pub fn new() -> Self {
        Self {
            snapshot: SnapshotTrie::default(),
            intermediate: IntermediateTrie::default(),
            delta: DeltaTrie::default(),
            snapshot_root: MERKLE_NULL_NODE,
            intermediate_padding: DeltaMptKeyPadding::genesis(),
            delta_padding: DeltaMptKeyPadding::genesis(),
            height: 0,
            snapshot_epoch_count: DEFAULT_SNAPSHOT_EPOCH_COUNT,
            last_root: None,
            delta_account_cache: Default::default(),
            intermediate_account_cache: Default::default(),
        }
    }

    pub fn with_snapshot_epoch_count(snapshot_epoch_count: u32) -> Self {
        Self {
            snapshot_epoch_count: snapshot_epoch_count.max(1),
            ..Self::new()
        }
    }

    pub fn from_snapshot(snapshot: BTreeMap<Vec<u8>, Box<[u8]>>) -> Self {
        let mut snapshot_inc = SnapshotTrie::from_snapshot(&snapshot);
        let snapshot_root = snapshot_inc.root();
        let delta_padding = DeltaMptKeyPadding::from_roots(snapshot_root, MERKLE_NULL_NODE);
        Self {
            snapshot: snapshot_inc,
            snapshot_root,
            delta_padding,
            ..Self::new()
        }
    }

    pub fn from_persisted(state: PersistedState) -> Self {
        let total = crate::diag::event("recover.from_persisted_total");

        let snapshot_entries = state.snapshot.len();
        let intermediate_entries = state.intermediate.len();
        let delta_entries = state.delta.len();

        let intermediate_map = state.intermediate;
        let delta_map = state.delta;

        let t = crate::diag::event("recover.snapshot_build");
        let mut snapshot_inc = SnapshotTrie::from_snapshot(&state.snapshot);
        t.finish(format_args!("entries={snapshot_entries}"));

        let t = crate::diag::event("recover.snapshot_root");
        let snapshot_root = snapshot_inc.root();
        t.finish(format_args!("entries={snapshot_entries}"));

        let t = crate::diag::event("recover.intermediate_root");
        let intermediate_root = trie_root(&intermediate_map);
        t.finish(format_args!("entries={intermediate_entries}"));

        let intermediate = IntermediateTrie::from_map(&intermediate_map, intermediate_root);
        let intermediate_padding = DeltaMptKeyPadding(state.intermediate_mpt_key_padding);
        let delta_padding = DeltaMptKeyPadding(state.delta_mpt_key_padding);

        // Build the single delta store from the loaded delta (one flat pass; the
        // first `root` warms the cache).
        let t = crate::diag::event("recover.delta_build");
        let delta = DeltaTrie::from_map(&delta_map);
        t.finish(format_args!("entries={delta_entries}"));

        total.finish(format_args!(
            "snapshot={snapshot_entries} intermediate={intermediate_entries} delta={delta_entries}"
        ));

        Self {
            snapshot: snapshot_inc,
            intermediate,
            delta,
            snapshot_root,
            intermediate_padding,
            delta_padding,
            last_root: state.last_root,
            height: state.height,
            snapshot_epoch_count: if state.snapshot_epoch_count == 0 {
                DEFAULT_SNAPSHOT_EPOCH_COUNT
            } else {
                state.snapshot_epoch_count
            },
            delta_account_cache: Default::default(),
            intermediate_account_cache: Default::default(),
        }
    }

    /// Construct a `State` from already-built layers, paddings and scalars
    /// (pure memory — no I/O). The derived roots (`snapshot_root` via the
    /// incremental snapshot root, `intermediate_root` via the stateless oracle)
    /// and the incremental delta trie are computed here. This is the
    /// reconstruction entry point used by [`crate::persist::stream::load_state`]
    /// after it has streamed the on-disk sections into memory.
    pub(crate) fn from_parts(
        mut snapshot: SnapshotTrie,
        intermediate: BTreeMap<Vec<u8>, MptValue>,
        delta: BTreeMap<Vec<u8>, MptValue>,
        intermediate_mpt_key_padding: [u8; 32],
        delta_mpt_key_padding: [u8; 32],
        height: u64,
        snapshot_epoch_count: u32,
        last_root: Option<CommitRoot>,
    ) -> Self {
        let t = crate::diag::event("recover.snapshot_root");
        let snapshot_root = snapshot.root();
        t.finish(format_args!("entries={}", snapshot.len()));

        let t = crate::diag::event("recover.intermediate_root");
        let intermediate_root = trie_root(&intermediate);
        t.finish(format_args!("entries={}", intermediate.len()));

        let intermediate = IntermediateTrie::from_map(&intermediate, intermediate_root);

        let t = crate::diag::event("recover.delta_build");
        let delta = DeltaTrie::from_map(&delta);
        t.finish(format_args!("entries={}", delta.len()));

        let intermediate_padding = DeltaMptKeyPadding(intermediate_mpt_key_padding);
        let delta_padding = DeltaMptKeyPadding(delta_mpt_key_padding);

        Self {
            snapshot,
            intermediate,
            delta,
            snapshot_root,
            intermediate_padding,
            delta_padding,
            last_root,
            height,
            snapshot_epoch_count: if snapshot_epoch_count == 0 {
                DEFAULT_SNAPSHOT_EPOCH_COUNT
            } else {
                snapshot_epoch_count
            },
            delta_account_cache: Default::default(),
            intermediate_account_cache: Default::default(),
        }
    }

    /// Reconstruct a `State` from a streamed checkpoint. Thin forwarder to
    /// [`crate::persist::stream::load_state`], which owns the I/O; `State` itself
    /// no longer reads. Kept for existing callers (`ckpt-bench`,
    /// replay-checkpoint, tests).
    pub fn from_reader(reader: &mut impl std::io::Read) -> std::io::Result<Self> {
        crate::persist::load_state(reader)
    }

    /// Stream this state to a checkpoint. Thin forwarder to
    /// [`crate::persist::stream::save_state`], which owns the I/O; `State` itself
    /// no longer writes. Kept for existing callers (`ckpt-bench`,
    /// replay-checkpoint, tests).
    pub fn write_streaming(&self, w: &mut impl std::io::Write) -> std::io::Result<()> {
        crate::persist::save_state(self, w)
    }

    // --- Pure-memory accessors used by `persist::stream` to stream each layer ---

    /// Number of live (non-tombstone) snapshot entries — the count framed ahead
    /// of the snapshot section on disk.
    pub(crate) fn snapshot_live_entry_count(&self) -> usize {
        self.snapshot.live_entry_count()
    }

    /// Visit each live snapshot entry as `(canonical key, value bytes)` in
    /// ascending key order.
    pub(crate) fn for_each_snapshot_entry(
        &self,
        f: impl FnMut(Vec<u8>, &[u8]) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        self.snapshot.for_each_canonical(f)
    }

    pub(crate) fn intermediate_entries(&self) -> &BTreeMap<Box<[u8]>, MptValue> {
        self.intermediate.raw_entries()
    }

    pub(crate) fn delta_entries(&self) -> &BTreeMap<Box<[u8]>, MptValue> {
        self.delta.raw_entries()
    }

    pub(crate) fn intermediate_padding_bytes(&self) -> [u8; 32] {
        self.intermediate_padding.0
    }

    pub(crate) fn delta_padding_bytes(&self) -> [u8; 32] {
        self.delta_padding.0
    }

    pub(crate) fn last_root(&self) -> &Option<CommitRoot> {
        &self.last_root
    }

    pub fn persisted(&self) -> PersistedState {
        PersistedState {
            snapshot: self.snapshot.to_canonical_map(),
            intermediate: self.intermediate.to_btreemap(),
            delta: self.delta.to_btreemap(),
            intermediate_mpt_key_padding: self.intermediate_padding.0,
            delta_mpt_key_padding: self.delta_padding.0,
            height: self.height,
            snapshot_epoch_count: self.snapshot_epoch_count,
            last_root: self.last_root.clone(),
        }
    }

    pub fn height(&self) -> u64 {
        self.height
    }

    pub fn snapshot_epoch_count(&self) -> u32 {
        self.snapshot_epoch_count
    }
}

impl StateTrait for State {
    fn get(&self, key: StorageKeyWithSpace) -> Result<Option<Box<[u8]>>> {
        let _t = profile::op(profile::Op::Read);

        let delta_key = {
            let mut cache = self.delta_account_cache.borrow_mut();
            key.to_delta_mpt_key_bytes(&self.delta_padding, Some(&mut *cache))?
        };
        if let Some(value) = self.delta.get(&delta_key) {
            return Ok(value.visible_value().map(Box::from));
        }
        let intermediate_key = {
            let mut cache = self.intermediate_account_cache.borrow_mut();
            key.to_delta_mpt_key_bytes(&self.intermediate_padding, Some(&mut *cache))?
        };
        if let Some(value) = self.intermediate.get(&intermediate_key) {
            return Ok(value.visible_value().map(Box::from));
        }
        let bytes = key.to_key_bytes()?;
        let _s = profile::op(profile::Op::SnapGet);
        Ok(self.snapshot.snapshot_get_owned(&bytes))
    }

    fn set(&mut self, key: StorageKeyWithSpace, value: Box<[u8]>) -> Result<()> {
        let _t = profile::op(profile::Op::Write);

        let raw = key.to_delta_mpt_key_bytes(
            &self.delta_padding,
            Some(self.delta_account_cache.get_mut()),
        )?;
        let value = if value.is_empty() {
            MptValue::Tombstone
        } else {
            MptValue::Some(value)
        };
        self.delta.insert(&raw, value);
        Ok(())
    }

    fn get_all_by_prefix(&self, prefix: StorageKeyWithSpace) -> Result<Option<Vec<MptKeyValue>>> {
        let values = self.read_prefix(prefix)?;
        Ok((!values.is_empty()).then_some(values))
    }

    fn delete_all_by_prefix(
        &mut self,
        prefix: StorageKeyWithSpace,
    ) -> Result<Option<Vec<MptKeyValue>>> {
        let values = self.delete_prefix(prefix)?;
        Ok((!values.is_empty()).then_some(values))
    }

    fn commit(&mut self) -> Result<CommitRoot> {
        commit::commit(self)
    }
}

#[cfg(test)]
mod tests;
