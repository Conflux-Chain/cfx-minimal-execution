use crate::{
    trie::MptValue,
    types::{CommitRoot, Result},
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs, path::PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedState {
    pub snapshot: BTreeMap<Vec<u8>, Box<[u8]>>,
    pub intermediate: BTreeMap<Vec<u8>, MptValue>,
    pub delta: BTreeMap<Vec<u8>, MptValue>,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mpt_value_bincode_encoding_matches_legacy_mpt_value_disk() {
        // MptValueDisk (deleted) was an identical enum: { Some(Box<[u8]>), Tombstone }.
        // bincode 1.x encodes enums as LE u32 variant_index + payload.
        // This test pins the exact byte layout so a future serde change is caught.

        // Variant 0 = Some, payload = length-prefixed bytes
        let mut expected_some = Vec::new();
        expected_some.extend_from_slice(&0u32.to_le_bytes());
        expected_some.extend_from_slice(&3u64.to_le_bytes());
        expected_some.extend_from_slice(&[1u8, 2, 3]);

        // Variant 1 = Tombstone, no payload
        let expected_tombstone = 1u32.to_le_bytes().to_vec();

        let actual_some = bincode::serialize(&MptValue::Some(Box::from([1u8, 2, 3]))).unwrap();
        let actual_tombstone = bincode::serialize(&MptValue::Tombstone).unwrap();

        assert_eq!(actual_some, expected_some, "Some encoding changed");
        assert_eq!(actual_tombstone, expected_tombstone, "Tombstone encoding changed");

        // Round-trip from the pinned bytes
        let rt_some: MptValue = bincode::deserialize(&expected_some).unwrap();
        let rt_tomb: MptValue = bincode::deserialize(&expected_tombstone).unwrap();
        assert!(matches!(rt_some, MptValue::Some(ref b) if b.as_ref() == [1, 2, 3]));
        assert!(matches!(rt_tomb, MptValue::Tombstone));
    }
}
