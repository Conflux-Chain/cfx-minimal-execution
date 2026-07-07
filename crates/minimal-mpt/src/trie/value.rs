#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum MptValue {
    Some(Box<[u8]>),
    Tombstone,
}

impl MptValue {
    pub fn visible_value(&self) -> Option<&[u8]> {
        match self {
            Self::Some(v) => Some(v),
            Self::Tombstone => None,
        }
    }

    pub(crate) fn merkle_value(&self) -> &[u8] {
        match self {
            Self::Some(v) => v,
            Self::Tombstone => &[],
        }
    }

    pub fn is_tombstone(&self) -> bool {
        matches!(self, Self::Tombstone)
    }
}

/// The snapshot's value type. The snapshot never stores a tombstone (deletes are
/// applied as removals — see [`crate::state::layers::snapshot::SnapshotTrie`]),
/// so it drops `MptValue`'s tombstone variant and inlines values up to 8 bytes:
/// the ~61% of snapshot values that are ≤8 B avoid a heap allocation, trading
/// +8 B of inline `BTreeMap` slot for the saved allocation. Merkle bytes are
/// byte-identical to `MptValue::Some`, so the snapshot root and checkpoint
/// format are unchanged.
pub type SnapshotValue = smallvec::SmallVec<u8, 8>;

/// Build a [`SnapshotValue`] from raw value bytes (always a live value).
pub(crate) fn snapshot_value(bytes: &[u8]) -> SnapshotValue {
    let mut v = SnapshotValue::new();
    v.extend_from_slice(bytes);
    v
}
