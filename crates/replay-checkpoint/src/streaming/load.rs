use crate::{legacy::Checkpoint, EpochCommitment, ExecutedEpoch, CHECKPOINT_VERSION};
use anyhow::{Context, Result};
use cfx_internal_common::StateRootWithAuxInfo;
use cfx_minimal_mpt::State as MmptState;
use cfx_types::H256;
use std::{collections::BTreeMap, fs, io::BufReader, path::Path};

/// Owned checkpoint state — the result of [`load`](Self::load) and the input
/// to executor resume.
pub struct CheckpointData {
    pub state: MmptState,
    pub height: u64,
    pub previous_epoch_hash: H256,
    pub previous_state_root: StateRootWithAuxInfo,
    pub previous_epoch_pos_view: Option<u64>,
    pub previous_epoch_finalized_epoch: Option<u64>,
    pub commitments: BTreeMap<u64, EpochCommitment>,
    pub executed_epochs: BTreeMap<u64, ExecutedEpoch>,
}

impl CheckpointData {
    /// Stream-read checkpoint fields from any `Read` source without
    /// materializing the full trie snapshot in memory.
    pub fn stream_read(r: &mut impl std::io::Read) -> Result<Self> {
        let height: u64 =
            bincode::deserialize_from(&mut *r).context("ckpt: height")?;
        let state = cfx_minimal_mpt::load_state(&mut *r)
            .context("ckpt: stream mmpt state")?;
        let previous_epoch_hash: H256 =
            bincode::deserialize_from(&mut *r).context("ckpt: prev_hash")?;
        let previous_state_root: StateRootWithAuxInfo =
            bincode::deserialize_from(&mut *r).context("ckpt: prev_root")?;
        let previous_epoch_pos_view: Option<u64> =
            bincode::deserialize_from(&mut *r).context("ckpt: pos_view")?;
        let previous_epoch_finalized_epoch: Option<u64> =
            bincode::deserialize_from(&mut *r).context("ckpt: finalized")?;
        let commitments: BTreeMap<u64, EpochCommitment> =
            bincode::deserialize_from(&mut *r).context("ckpt: commitments")?;
        let executed_epochs: BTreeMap<u64, ExecutedEpoch> =
            bincode::deserialize_from(&mut *r).context("ckpt: exec_epochs")?;

        Ok(CheckpointData {
            state,
            height,
            previous_epoch_hash,
            previous_state_root,
            previous_epoch_pos_view,
            previous_epoch_finalized_epoch,
            commitments,
            executed_epochs,
        })
    }

    /// Load a checkpoint from a file. Returns `Ok(None)` when the file does
    /// not exist. Handles version dispatch (V1 falls back to legacy load).
    pub fn load(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let file = fs::File::open(path)
            .with_context(|| format!("open checkpoint {}", path.display()))?;
        let mut r = BufReader::new(file);

        let version: u32 = bincode::deserialize_from(&mut r)
            .context("read checkpoint version")?;
        match version {
            1 => {
                let ckpt = Checkpoint::load(path)?.expect("path exists");
                Ok(Some(ckpt.into_data()?))
            }
            CHECKPOINT_VERSION => Ok(Some(Self::stream_read(&mut r)?)),
            _ => anyhow::bail!(
                "checkpoint version mismatch: file v{version}, expected v{CHECKPOINT_VERSION}",
            ),
        }
    }
}
