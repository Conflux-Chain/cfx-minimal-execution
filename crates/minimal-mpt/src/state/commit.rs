//! The state commit flow, split out of `state/mod.rs`. `StateTrait::commit`
//! forwards to [`commit`] here: the single place that folds the incremental delta
//! root into a [`CommitRoot`] and advances the layers.

use super::{CommitRoot, State};
use crate::diag::profile;
#[cfg(any(test, feature = "verify-incremental"))]
use crate::trie::trie_root;
use crate::Result;

pub(super) fn commit(state: &mut State) -> Result<CommitRoot> {
    let mut t = profile::op(profile::Op::DeltaRoot);
    t.entries = state.delta.len() as u64;
    let delta_root = state.delta.root();
    drop(t);

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
    if profile::on()
        && state
            .height
            .is_multiple_of(state.snapshot_epoch_count as u64)
    {
        profile::dump(&format!("cumulative @height={}", state.height));
    }
    Ok(root)
}
