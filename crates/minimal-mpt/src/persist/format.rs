use crate::{state::CommitRoot, trie::MptValue};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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
