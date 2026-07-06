//! Non-streaming (full-materialization) save and load — the ground-truth
//! reference implementation. Production code uses the streaming paths in
//! `streaming/`; this module exists for verification and tests.

#[cfg(test)]
use crate::ExecutedEpoch;
use crate::{CheckpointData, CheckpointRef, EpochCommitment, CHECKPOINT_VERSION};
use anyhow::{Context, Result};
use cfx_internal_common::StateRootWithAuxInfo;
use cfx_minimal_mpt::{PersistedState, State as MmptState};
use cfx_types::H256;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::{BufWriter, Write},
    path::Path,
};

// ── On-disk format structs ──────────────────────────────────────────────

#[derive(Deserialize)]
pub(crate) struct CheckpointV1 {
    #[allow(dead_code)]
    version: u32,
    height: u64,
    mmpt: PersistedState,
    previous_epoch_hash: H256,
    previous_state_root: StateRootWithAuxInfo,
    commitments: Vec<(u64, EpochCommitment)>,
    executed_epochs: Vec<(u64, crate::ExecutedEpoch)>,
}

/// The on-disk checkpoint format. Defines the bincode field order that both
/// streaming and legacy paths must agree on. Not part of the public API —
/// production code uses [`CheckpointData`] and [`CheckpointRef`].
#[derive(Serialize, Deserialize)]
pub(crate) struct Checkpoint {
    version: u32,
    height: u64,
    mmpt: PersistedState,
    previous_epoch_hash: H256,
    previous_state_root: StateRootWithAuxInfo,
    previous_epoch_pos_view: Option<u64>,
    previous_epoch_finalized_epoch: Option<u64>,
    commitments: Vec<(u64, EpochCommitment)>,
    executed_epochs: Vec<(u64, crate::ExecutedEpoch)>,
}

// ── Checkpoint methods ──────────────────────────────────────────────────

impl Checkpoint {
    #[cfg(test)]
    pub(crate) fn build(
        mmpt: PersistedState, previous_epoch_hash: H256,
        previous_state_root: &StateRootWithAuxInfo,
        previous_epoch_pos_view: Option<u64>,
        previous_epoch_finalized_epoch: Option<u64>,
        commitments: &BTreeMap<u64, EpochCommitment>,
        executed_epochs: &BTreeMap<u64, ExecutedEpoch>,
    ) -> Self {
        Self {
            version: CHECKPOINT_VERSION,
            height: mmpt.height,
            mmpt,
            previous_epoch_hash,
            previous_state_root: previous_state_root.clone(),
            previous_epoch_pos_view,
            previous_epoch_finalized_epoch,
            commitments: commitments.iter().map(|(h, c)| (*h, *c)).collect(),
            executed_epochs: executed_epochs
                .iter()
                .map(|(h, d)| (*h, d.clone()))
                .collect(),
        }
    }

    pub(crate) fn height(&self) -> u64 {
        self.height
    }

    /// Full bincode save (materializes entire trie in the serialization buffer).
    pub(crate) fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("create checkpoint dir {}", parent.display())
            })?;
        }
        let tmp = path.with_extension("tmp");
        let file = fs::File::create(&tmp).with_context(|| {
            format!("create checkpoint tmp {}", tmp.display())
        })?;
        let mut w = BufWriter::new(file);
        bincode::serialize_into(&mut w, self)
            .context("serialize replay checkpoint")?;
        w.flush().context("flush checkpoint")?;
        drop(w);
        fs::rename(&tmp, path).with_context(|| {
            format!("rename checkpoint into {}", path.display())
        })?;
        Ok(())
    }

    /// Full bincode load with transparent V1 upgrade.
    pub(crate) fn load(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(path)
            .with_context(|| format!("read checkpoint {}", path.display()))?;
        if bytes.len() < 4 {
            anyhow::bail!("checkpoint file too short");
        }
        let version: u32 = bincode::deserialize(&bytes[..4])
            .context("read checkpoint version")?;
        let ckpt = match version {
            1 => {
                let v1: CheckpointV1 = bincode::deserialize(&bytes)
                    .with_context(|| format!("deserialize v1 checkpoint {}", path.display()))?;
                Self {
                    version: CHECKPOINT_VERSION,
                    height: v1.height,
                    mmpt: v1.mmpt,
                    previous_epoch_hash: v1.previous_epoch_hash,
                    previous_state_root: v1.previous_state_root,
                    previous_epoch_pos_view: None,
                    previous_epoch_finalized_epoch: None,
                    commitments: v1.commitments,
                    executed_epochs: v1.executed_epochs,
                }
            }
            CHECKPOINT_VERSION => {
                bincode::deserialize(&bytes)
                    .with_context(|| format!("deserialize checkpoint {}", path.display()))?
            }
            _ => anyhow::bail!(
                "checkpoint version mismatch: file v{version}, expected v{CHECKPOINT_VERSION}",
            ),
        };
        Ok(Some(ckpt))
    }

    /// Convert into live data (building trie State from PersistedState).
    pub(crate) fn into_data(self) -> Result<CheckpointData> {
        let commitments = self.commitments.into_iter().collect();
        let executed_epochs = self.executed_epochs.into_iter().collect();
        Ok(CheckpointData {
            state: MmptState::from_persisted(self.mmpt),
            height: self.height,
            previous_epoch_hash: self.previous_epoch_hash,
            previous_state_root: self.previous_state_root,
            previous_epoch_pos_view: self.previous_epoch_pos_view,
            previous_epoch_finalized_epoch: self.previous_epoch_finalized_epoch,
            commitments,
            executed_epochs,
        })
    }

    /// Streaming save from the owned `mmpt` — clones it into a live `State`
    /// then delegates to [`CheckpointRef::save_streaming`]. Used by
    /// [`verify_roundtrip`](crate::verify_roundtrip) to prove the streaming
    /// output is byte-identical to [`save`](Self::save).
    pub(crate) fn save_streaming_self(&self, path: &Path) -> Result<()> {
        let state = MmptState::from_persisted(self.mmpt.clone());
        let commitments: BTreeMap<u64, EpochCommitment> =
            self.commitments.iter().map(|(h, c)| (*h, *c)).collect();
        let executed_epochs: BTreeMap<u64, crate::ExecutedEpoch> =
            self.executed_epochs.iter().map(|(h, e)| (*h, e.clone())).collect();
        CheckpointRef {
            state: &state,
            previous_epoch_hash: self.previous_epoch_hash,
            previous_state_root: &self.previous_state_root,
            previous_epoch_pos_view: self.previous_epoch_pos_view,
            previous_epoch_finalized_epoch: self.previous_epoch_finalized_epoch,
            commitments: &commitments,
            executed_epochs: &executed_epochs,
        }
        .save(path)
    }
}
