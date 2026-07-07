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

/// A value stored in an [`crate::trie::IncrementalTrie`] node, abstracted
/// so the (dominant) snapshot trie can use a compact inline representation while
/// the delta keeps the tombstone-carrying [`MptValue`]. The snapshot never holds
/// a tombstone (deletes are applied as removals), so its value type can drop the
/// tombstone variant entirely — see the snapshot module.
pub trait TrieValue {
    /// Bytes hashed for a key terminating at this node (empty for a tombstone).
    fn merkle_value(&self) -> &[u8];
    /// Live value bytes, or `None` for a tombstone (logically absent).
    fn visible_value(&self) -> Option<&[u8]>;
    /// Build from raw value bytes — always a present/live value.
    fn from_value_bytes(bytes: &[u8]) -> Self;
}

impl TrieValue for MptValue {
    fn merkle_value(&self) -> &[u8] {
        MptValue::merkle_value(self)
    }
    fn visible_value(&self) -> Option<&[u8]> {
        MptValue::visible_value(self)
    }
    fn from_value_bytes(bytes: &[u8]) -> Self {
        MptValue::Some(Box::from(bytes))
    }
}
