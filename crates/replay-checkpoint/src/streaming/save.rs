use crate::{EpochCommitment, ExecutedEpoch, CHECKPOINT_VERSION};
use anyhow::{Context, Result};
use cfx_internal_common::StateRootWithAuxInfo;
use cfx_minimal_mpt::State as MmptState;
use cfx_types::H256;
use std::{
    collections::BTreeMap,
    fs,
    io::{BufWriter, Write},
    path::Path,
};

/// Borrowed view of executor state for zero-copy streaming save.
pub struct CheckpointRef<'a> {
    pub state: &'a MmptState,
    pub previous_epoch_hash: H256,
    pub previous_state_root: &'a StateRootWithAuxInfo,
    pub previous_epoch_pos_view: Option<u64>,
    pub previous_epoch_finalized_epoch: Option<u64>,
    pub commitments: &'a BTreeMap<u64, EpochCommitment>,
    pub executed_epochs: &'a BTreeMap<u64, ExecutedEpoch>,
}

impl CheckpointRef<'_> {
    /// Stream the checkpoint into any `Write` sink without materializing the
    /// full trie snapshot in memory.
    pub fn stream_write(&self, w: &mut impl Write) -> Result<()> {
        bincode::serialize_into(&mut *w, &CHECKPOINT_VERSION)
            .context("ckpt: version")?;
        bincode::serialize_into(&mut *w, &self.state.height())
            .context("ckpt: height")?;
        self.state
            .write_streaming(&mut *w)
            .context("ckpt: stream mmpt state")?;
        bincode::serialize_into(&mut *w, &self.previous_epoch_hash)
            .context("ckpt: prev_hash")?;
        bincode::serialize_into(&mut *w, self.previous_state_root)
            .context("ckpt: prev_root")?;
        bincode::serialize_into(&mut *w, &self.previous_epoch_pos_view)
            .context("ckpt: pos_view")?;
        bincode::serialize_into(&mut *w, &self.previous_epoch_finalized_epoch)
            .context("ckpt: finalized")?;
        bincode::serialize_into(&mut *w, self.commitments)
            .context("ckpt: commitments")?;
        bincode::serialize_into(&mut *w, self.executed_epochs)
            .context("ckpt: exec_epochs")?;
        Ok(())
    }

    /// Write the checkpoint to a file (creates parent dirs if needed).
    /// Does NOT do atomic tmp+rename — the caller is responsible for
    /// atomicity if needed.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("create checkpoint dir {}", parent.display())
            })?;
        }
        let file = fs::File::create(path).with_context(|| {
            format!("create checkpoint {}", path.display())
        })?;
        let mut w = BufWriter::new(file);
        self.stream_write(&mut w)?;
        w.flush().context("flush checkpoint")?;
        Ok(())
    }
}
