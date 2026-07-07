//! The state commit flow, split out of `state/mod.rs`. `StateTrait::commit`
//! forwards to [`commit`] here: the single place that folds the incremental delta
//! root into a [`CommitRoot`] and advances the layers.

use super::{CommitRoot, State, DELTA_ROOT_CNT, DELTA_ROOT_NS};
#[cfg(any(test, feature = "verify-incremental"))]
use crate::trie::trie_root;
use crate::Result;

pub(super) fn commit(state: &mut State) -> Result<CommitRoot> {
    let delta_root = if crate::diag::delta_timing() {
        use std::sync::atomic::Ordering::Relaxed;
        let t = std::time::Instant::now();
        let r = state.delta.root();
        let ns = t.elapsed().as_nanos() as u64;
        let sum = DELTA_ROOT_NS.fetch_add(ns, Relaxed) + ns;
        let cnt = DELTA_ROOT_CNT.fetch_add(1, Relaxed) + 1;
        if cnt.is_multiple_of(20_000) {
            eprintln!(
                "[delta-root] commits={cnt} avg={}us total={}ms last={}us N={}",
                sum / cnt / 1000,
                sum / 1_000_000,
                ns / 1000,
                state.delta.len(),
            );
        }
        r
    } else {
        state.delta.root()
    };
    // Cross-check against the stateless oracle, but only under tests / the
    // `verify-incremental` feature (fuzzing). NOT `debug_assert!`: this
    // workspace builds release with `debug-assertions = true`, so a
    // debug_assert here would run the full O(N) `trie_root` every commit in
    // the replay and erase the incremental speedup.
    #[cfg(any(test, feature = "verify-incremental"))]
    assert_eq!(
        delta_root,
        trie_root(&state.delta.to_btreemap()),
        "incremental delta root diverged from stateless trie_root"
    );
    let root = CommitRoot::new(
        state.snapshot_root,
        state.intermediate.root(),
        delta_root,
        state.delta_padding.0,
    );
    state.last_root = Some(root.clone());
    state.height += 1;
    state.advance_after_commit(delta_root)?;
    Ok(root)
}
