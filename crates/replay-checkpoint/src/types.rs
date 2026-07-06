//! Replay domain types that travel with the checkpoint.

use primitives::receipt::BlockReceipts;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// The commitment an epoch produces: the three roots we recompute and later
/// compare (DEFERRED_STATE_EPOCH_COUNT epochs on) against the chain's pivot.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct EpochCommitment {
    pub state_root: cfx_types::H256,
    pub receipts_root: cfx_types::H256,
    pub logs_bloom_hash: cfx_types::H256,
}

/// One executed epoch retained until its reward settles REWARD_EPOCH_COUNT
/// epochs later.
///
/// `BlockReceipts` has no serde — only RLP. The `receipts` field uses a custom
/// serializer that encodes each receipt as RLP bytes, keeping the on-disk
/// format identical to `Vec<Vec<u8>>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutedEpoch {
    pub blocks: Vec<cfxpack::packet::Block>,
    #[serde(
        serialize_with = "receipts_serde::serialize",
        deserialize_with = "receipts_serde::deserialize"
    )]
    pub receipts: Vec<Arc<BlockReceipts>>,
}

mod receipts_serde {
    use super::*;

    pub fn serialize<S: serde::Serializer>(
        receipts: &[Arc<BlockReceipts>],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let rlp_bytes: Vec<Vec<u8>> = receipts
            .iter()
            .map(|r| rlp::encode(&**r).to_vec())
            .collect();
        rlp_bytes.serialize(serializer)
    }

    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<Arc<BlockReceipts>>, D::Error> {
        let rlp_bytes: Vec<Vec<u8>> = Vec::deserialize(deserializer)?;
        rlp_bytes
            .iter()
            .map(|bytes| {
                rlp::decode::<BlockReceipts>(bytes)
                    .map(Arc::new)
                    .map_err(serde::de::Error::custom)
            })
            .collect()
    }
}
