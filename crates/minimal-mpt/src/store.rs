use crate::{
    trie::MptValue,
    types::{CommitRoot, Result},
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs, path::PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedState {
    pub snapshot: BTreeMap<Vec<u8>, Box<[u8]>>,
    pub intermediate: BTreeMap<Vec<u8>, MptValueDisk>,
    pub delta: BTreeMap<Vec<u8>, MptValueDisk>,
    pub intermediate_mpt_key_padding: [u8; 32],
    pub delta_mpt_key_padding: [u8; 32],
    pub height: u64,
    pub snapshot_epoch_count: u32,
    pub last_root: Option<CommitRoot>,
}

impl Default for PersistedState {
    fn default() -> Self {
        Self {
            snapshot: BTreeMap::new(),
            intermediate: BTreeMap::new(),
            delta: BTreeMap::new(),
            intermediate_mpt_key_padding: crate::DeltaMptKeyPadding::genesis().0,
            delta_mpt_key_padding: crate::DeltaMptKeyPadding::genesis().0,
            height: 0,
            snapshot_epoch_count: crate::state::DEFAULT_SNAPSHOT_EPOCH_COUNT,
            last_root: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MptValueDisk {
    Some(Box<[u8]>),
    Tombstone,
}

impl From<MptValue> for MptValueDisk {
    fn from(value: MptValue) -> Self {
        match value {
            MptValue::Some(v) => Self::Some(v),
            MptValue::Tombstone => Self::Tombstone,
        }
    }
}

impl From<MptValueDisk> for MptValue {
    fn from(value: MptValueDisk) -> Self {
        match value {
            MptValueDisk::Some(v) => Self::Some(v),
            MptValueDisk::Tombstone => Self::Tombstone,
        }
    }
}

pub trait StateStore {
    fn load_latest(&self) -> Result<Option<PersistedState>>;
    fn save_latest(&mut self, state: &PersistedState) -> Result<()>;
}

#[derive(Default)]
pub struct MemoryStore {
    state: Option<PersistedState>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl StateStore for MemoryStore {
    fn load_latest(&self) -> Result<Option<PersistedState>> {
        Ok(self.state.clone())
    }

    fn save_latest(&mut self, state: &PersistedState) -> Result<()> {
        self.state = Some(state.clone());
        Ok(())
    }
}

pub struct FileStore {
    path: PathBuf,
}

impl FileStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl StateStore for FileStore {
    fn load_latest(&self) -> Result<Option<PersistedState>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&self.path)?;
        Ok(Some(bincode::deserialize(&bytes)?))
    }

    fn save_latest(&mut self, state: &PersistedState) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("tmp");
        fs::write(&tmp, bincode::serialize(state)?)?;
        fs::rename(tmp, &self.path)?;
        Ok(())
    }
}
