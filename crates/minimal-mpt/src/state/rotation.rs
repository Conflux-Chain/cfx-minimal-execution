//! Period-boundary rotation for the three-layer state.
//!
//! At each snapshot-period boundary the layers shift down by one: the snapshot
//! absorbs the intermediate (an in-place incremental re-root), the just-finished
//! delta becomes the new intermediate, and the delta starts fresh. This is the
//! only place the snapshot trie mutates.

use super::State;
use crate::diag::profile;
use crate::{
    keys::{DeltaMptKeyPadding, StorageKeyWithSpace},
    primitives::H256,
    state::layers::intermediate::IntermediateTrie,
    Result,
};

impl State {
    pub(super) fn advance_after_commit(&mut self, delta_root: H256) -> Result<()> {
        if self.height == 0 || !self.height.is_multiple_of(self.snapshot_epoch_count as u64) {
            return Ok(());
        }

        // Merge snapshot(N+1) = merge(snapshot(N), intermediate(N)) IN PLACE on
        // this (the only) thread: absorb the intermediate (re-keyed delta-mpt →
        // canonical) into the snapshot trie, then take the incremental root —
        // which only re-hashes the frontier the intermediate touched. No clone,
        // no background thread: the incremental root is cheap enough (~tens of
        // ms) to fold straight into the boundary. `take` empties intermediate so
        // the snapshot can be mutated without aliasing it.
        let mut m = profile::op(profile::Op::Merge);

        let t = crate::diag::event("merge.apply");
        let old_intermediate = std::mem::take(&mut self.intermediate);
        m.entries = old_intermediate.len() as u64;
        let mut snapshot_updates = Vec::with_capacity(old_intermediate.len());
        for (raw_key, value) in old_intermediate.into_entries() {
            let canonical = StorageKeyWithSpace::from_delta_mpt_key(&raw_key)?.to_key_bytes()?;
            snapshot_updates.push((canonical, value));
        }
        self.snapshot.apply_updates(snapshot_updates);
        t.finish(format_args!("h={} entries={}", self.height, m.entries));

        let t = crate::diag::event("merge.hash");
        self.snapshot_root = self.snapshot.root();
        t.finish(format_args!("h={} N={}", self.height, self.snapshot.len()));

        drop(m);

        // Rotate: the just-finished delta becomes the new intermediate. Its store
        // is MOVED in as-is (no re-materialization) and the remembered delta root
        // becomes the intermediate root; the delta restarts empty (with a fresh
        // engine — the padding below changes the key space, so the old cached delta
        // subtree hashes could not apply anyway).
        let finished_delta = std::mem::take(&mut self.delta);
        self.intermediate = IntermediateTrie::from_store(finished_delta.into_store(), delta_root);
        self.intermediate_padding = self.delta_padding.clone();
        self.delta_padding =
            DeltaMptKeyPadding::from_roots(self.snapshot_root, self.intermediate.root());
        // Rotate the account-key caches to match the padding rotation above: the
        // new intermediate padding equals the old delta padding, so the old delta
        // cache is exactly valid as the new intermediate cache (and the padding
        // stamp would otherwise discard it). Delta starts fresh.
        *self.intermediate_account_cache.get_mut() =
            std::mem::take(self.delta_account_cache.get_mut());
        Ok(())
    }
}
