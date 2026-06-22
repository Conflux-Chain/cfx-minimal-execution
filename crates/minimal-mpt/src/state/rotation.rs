//! Period-boundary rotation for the three-layer state.
//!
//! At each snapshot-period boundary the layers shift down by one: the snapshot
//! absorbs the intermediate (an in-place incremental re-root), the just-finished
//! delta becomes the new intermediate, and the delta starts fresh. This is the
//! only place the snapshot trie mutates.

use super::State;
use crate::{
    key_codec::{DeltaMptKeyPadding, StorageKeyWithSpace},
    trie::MptValue,
    types::{Result, H256},
};

/// Env-gated (`MMPT_MERGE_TIMING=1`) switch for the in-place `[merge]`
/// instrumentation in [`State::advance_after_commit`]. Cached; zero overhead
/// when unset.
static MMPT_TIMING: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
fn timing_on() -> bool {
    *MMPT_TIMING.get_or_init(|| std::env::var_os("MMPT_MERGE_TIMING").is_some())
}

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
        let timing = timing_on();
        let t0 = std::time::Instant::now();
        for (raw_key, value) in std::mem::take(&mut self.intermediate) {
            let canonical = StorageKeyWithSpace::from_delta_mpt_key(&raw_key)?.to_key_bytes()?;
            match value {
                MptValue::Some(value) => self.snapshot.insert(&canonical, MptValue::Some(value)),
                MptValue::Tombstone => self.snapshot.remove(&canonical),
            }
        }
        let apply_ms = t0.elapsed().as_millis();
        let t1 = std::time::Instant::now();
        self.snapshot_root = self.snapshot.root();
        let hash_ms = t1.elapsed().as_millis();
        if timing {
            eprintln!(
                "[merge] h={} N={} apply={}ms hash={}ms merge_total={}ms",
                self.height,
                self.snapshot.len(),
                apply_ms,
                hash_ms,
                apply_ms + hash_ms,
            );
        }

        // Rotate: the just-finished delta becomes the new intermediate. Delta is
        // now empty (and the padding below changes the key space), so the cached
        // delta subtree hashes no longer apply.
        self.intermediate = std::mem::take(&mut self.delta);
        self.delta_inc.clear();
        self.intermediate_root = delta_root;
        self.intermediate_padding = self.delta_padding.clone();
        self.delta_padding =
            DeltaMptKeyPadding::from_roots(self.snapshot_root, self.intermediate_root);
        // Rotate the account-key caches to match the padding rotation above: the
        // new intermediate padding equals the old delta padding, so the old delta
        // cache is exactly valid as the new intermediate cache (and the padding
        // stamp would otherwise discard it). Delta starts fresh.
        *self.intermediate_account_cache.get_mut() =
            std::mem::take(self.delta_account_cache.get_mut());
        Ok(())
    }
}
