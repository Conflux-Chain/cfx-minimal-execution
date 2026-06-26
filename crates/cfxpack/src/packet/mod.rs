//! The `.cfxpack` packet header types, flag constants, and the encoder entry
//! point. The byte-level encoding lives in [`encode`]; decoding is in
//! [`crate::decode`], structural verification in [`crate::verify`].

use cfx_types::{Address, H256, U256};
use primitives::SignedTransaction;
use serde::{Deserialize, Serialize};

mod encode;
pub use encode::encode_packet;

pub const PACKET_EPOCHS: u64 = 2000;
pub const HEADER_FIXED_LEN: usize = 93;
pub const HEADER_OFFSET_COUNT: usize = 8;
pub const HEADER_LEN: usize = HEADER_FIXED_LEN + HEADER_OFFSET_COUNT * 4;
pub const FLAG_ADAPTIVE: u8 = 1 << 0;
pub const FLAG_PIVOT: u8 = 1 << 1;
pub const FLAG_ESPACE: u8 = 1 << 2;
pub const FLAG_HAS_TRANSACTIONS: u8 = 1 << 3;
pub const FLAG_TX_COMPRESSED: u8 = 1 << 4;
pub const FLAG_SKIPPED_EXECUTION: u8 = 1 << 5;
/// Set when the block's `total_reward` (the full settled reward — distinct from
/// `base_reward`) is zero. A corner-case marker; bit 7 stays reserved.
pub const FLAG_ZERO_TOTAL_REWARD: u8 = 1 << 6;

#[derive(Debug, Clone)]
pub struct Packet {
    pub prev_last_hash: H256,
    pub prev_last_deferred_state_root: H256,
    pub first_block_number: u64,
    pub min_timestamp: u64,
    pub min_height: u64,
    pub min_pos_height: u64,
    pub addresses: Vec<Address>,
    pub pos_entries: Vec<PosLookupEntry>,
    pub difficulties: Vec<U256>,
    pub sender_base_nonces: Vec<SenderBaseNonce>,
    pub gas_prices: Vec<U256>,
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone)]
pub struct PosLookupEntry {
    pub hash: H256,
    pub height_offset: u16,
}

#[derive(Debug, Clone)]
pub struct SenderBaseNonce {
    pub sender_index: usize,
    pub base_nonce: u64,
}

/// One account's share of a PoS interest distribution, mirroring production
/// `PosRewardForAccount` (the already-computed final amount, not raw points).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PosRewardAccount {
    pub address: Address,
    pub pos_identifier: H256,
    pub reward: U256,
}

/// A PoS interest distribution event, mirroring production `PosRewardInfo`
/// (`crates/cfxcore/types/.../block_data_types.rs`). The extractor reads it from
/// the PoW `reward_by_pos_epoch` CF and attributes it to the PoW epoch whose
/// pivot hash equals `execution_epoch_hash`; it is carried in that epoch's last
/// block's tx segment (tag-3 item) and applied at epoch settlement. See
/// DESIGN.md §8.8.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PosRewardEntry {
    pub account_rewards: Vec<PosRewardAccount>,
    pub execution_epoch_hash: H256,
}

/// A PoS node unlock event, mirroring production `UnlockEvent`. The extractor
/// reads it from the pos-ledger-db `event` CF and attributes it to the PoW
/// epoch where `pos_reference` changed; it is carried in that epoch's last
/// block's tx segment (tag-4 item). The executor calls `update_pos_status` to
/// adjust `TotalPosStaking`. See DESIGN.md §8.8.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnlockEntry {
    pub identifier: H256,
    pub unlocked: u64,
}

// `Serialize`/`Deserialize` are required by the executor's resume checkpoint
// (which persists `Vec<Block>`); harmless to the extractor, which never
// serializes it. `primitives` derives serde on the transaction types, so this
// derives cleanly under both consumers' dependency sources.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Block {
    pub epoch: u64,
    pub index: usize,
    pub hash: H256,
    pub deferred_state_root: H256,
    pub deferred_receipts_root: H256,
    pub deferred_logs_bloom_hash: H256,
    pub flags: u8,
    pub author: Address,
    pub timestamp: u64,
    pub difficulty: U256,
    pub gas_limit: U256,
    pub base_price_core: U256,
    pub base_price_espace: U256,
    pub height: u64,
    pub blame: u64,
    pub finalized_epoch: u64,
    pub base_reward: U256,
    pub transactions: Vec<SignedTransaction>,
    pub transaction_refs: Vec<Option<(usize, usize)>>,
    /// PoS interest distributions attributed to this block's epoch (DESIGN §8.8).
    /// Populated by the decoder from tag-3 tx-segment items; the executor applies
    /// them at epoch settlement. `serde(skip)`: applied at execution time and
    /// never needed afterwards, so it is not persisted in the resume checkpoint —
    /// this keeps the checkpoint binary format unchanged (old checkpoints still
    /// load).
    #[serde(skip)]
    pub pos_rewards: Vec<PosRewardEntry>,
    /// PoS node unlock events attributed to this block's epoch (DESIGN §8.8).
    /// Populated by the decoder from tag-4 tx-segment items; the executor calls
    /// `update_pos_status` to adjust `TotalPosStaking`.
    #[serde(skip)]
    pub unlock_events: Vec<UnlockEntry>,
    /// PoS view number derived from this block's `pos_reference`. Not part of
    /// the wire format: populated by the decoder from the Packet's `pos_entries`
    /// table, or by the extractor from the PoS DB. `None` before PoS activation.
    #[serde(skip)]
    pub pos_view: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{decode::{decode_packet, decode_packet_ext}, verify::verify_packet};
    use cfx_types::AddressSpaceUtil;
    use primitives::transaction::NativeTransaction;

    #[test]
    fn raw_to_packet_roundtrip_minimal_blocks() {
        let author = Address::from_low_u64_be(1);
        let raw = Packet {
            prev_last_hash: H256::from_low_u64_be(9),
            prev_last_deferred_state_root: H256::from_low_u64_be(10),
            first_block_number: 100,
            min_timestamp: 1_700_000_000,
            min_height: 42,
            min_pos_height: 0,
            addresses: vec![author],
            pos_entries: Vec::new(),
            difficulties: vec![U256::from(1000)],
            sender_base_nonces: Vec::new(),
            gas_prices: Vec::new(),
            blocks: vec![
                Block {
                    epoch: 45,
                    index: 0,
                    hash: H256::from_low_u64_be(11),
                    deferred_state_root: H256::from_low_u64_be(12),
                    deferred_receipts_root: H256::from_low_u64_be(13),
                    deferred_logs_bloom_hash: H256::from_low_u64_be(14),
                    flags: FLAG_ADAPTIVE,
                    author,
                    timestamp: 1_700_000_000,
                    difficulty: U256::from(1000),
                    gas_limit: U256::from(30_000_000),
                    base_price_core: U256::zero(),
                    base_price_espace: U256::zero(),
                    height: 42,
                    blame: 0,
                    finalized_epoch: 0,
                    base_reward: U256::from(1),
                    transactions: Vec::new(),
                    transaction_refs: Vec::new(),
                    pos_rewards: Vec::new(),
                    unlock_events: Vec::new(),
                    pos_view: None,
                },
                Block {
                    epoch: 45,
                    index: 1,
                    hash: H256::from_low_u64_be(15),
                    deferred_state_root: H256::from_low_u64_be(16),
                    deferred_receipts_root: H256::from_low_u64_be(17),
                    deferred_logs_bloom_hash: H256::from_low_u64_be(18),
                    flags: FLAG_PIVOT | FLAG_ESPACE,
                    author,
                    timestamp: 1_700_000_005,
                    difficulty: U256::from(1000),
                    gas_limit: U256::from(60_000_000),
                    base_price_core: U256::from(1_000_000_000),
                    base_price_espace: U256::from(20_000_000_000u64),
                    height: 45,
                    blame: 1,
                    finalized_epoch: 5,
                    base_reward: U256::from(2),
                    transactions: Vec::new(),
                    transaction_refs: Vec::new(),
                    pos_rewards: Vec::new(),
                    unlock_events: Vec::new(),
                    pos_view: Some(7),
                },
            ],
        };

        let pos_h = 45;
        let packet = encode_packet(&raw).expect("encode raw packet");
        let report = verify_packet(&packet).expect("verify packet");
        assert_eq!(report.block_count, 2);
        assert_eq!(report.transaction_blocks, 0);
        assert_eq!(report.first_block_number, 100);
        assert!(matches!(report.block_prefix_size, 64 | 72 | 80 | 88 | 96));

        let decoded = decode_packet_ext(&packet, pos_h).expect("decode packet");
        assert_eq!(decoded.blocks[0].height, 42);
        assert_eq!(decoded.blocks[0].epoch, 45);
        assert_eq!(decoded.blocks[0].pos_view, None);
        assert_eq!(decoded.blocks[1].height, 45);
        assert_eq!(decoded.blocks[1].epoch, 45);
        assert_eq!(decoded.blocks[1].pos_view, Some(7));
        let reencoded = encode_packet(&decoded).expect("reencode decoded packet");
        assert_eq!(reencoded, packet);
        let reencoded_report = verify_packet(&reencoded).expect("verify reencoded packet");
        assert_eq!(reencoded_report.block_count, report.block_count);
        assert_eq!(reencoded_report.transaction_items, report.transaction_items);
    }

    #[test]
    fn raw_to_packet_roundtrip_with_transaction_payload() {
        let author = Address::from_low_u64_be(1);
        let sender = Address::from_low_u64_be(2);
        let receiver = Address::from_low_u64_be(3);
        let tx = NativeTransaction {
            nonce: U256::from(7),
            gas_price: U256::from(1_000_000_000u64),
            gas: U256::from(21_000),
            action: primitives::Action::Call(receiver),
            value: U256::from(42),
            storage_limit: 0,
            // Below the block epoch (1000): a negative offset whose sign must
            // survive the round-trip. A zero/positive delta would not catch the
            // abs_diff sign-loss regression.
            epoch_height: 900,
            chain_id: 1029,
            data: Vec::new().into(),
        }
        .fake_sign(sender.with_native_space());

        let raw = Packet {
            prev_last_hash: H256::from_low_u64_be(9),
            prev_last_deferred_state_root: H256::from_low_u64_be(10),
            first_block_number: 1000,
            min_timestamp: 1_700_000_000,
            min_height: 1000,
            min_pos_height: 0,
            addresses: vec![author, sender, receiver],
            pos_entries: Vec::new(),
            difficulties: vec![U256::from(1000)],
            sender_base_nonces: Vec::new(),
            gas_prices: vec![U256::from(1_000_000_000u64)],
            blocks: vec![Block {
                epoch: 1000,
                index: 0,
                hash: H256::from_low_u64_be(11),
                deferred_state_root: H256::from_low_u64_be(12),
                deferred_receipts_root: H256::from_low_u64_be(13),
                deferred_logs_bloom_hash: H256::from_low_u64_be(14),
                flags: FLAG_PIVOT,
                author,
                timestamp: 1_700_000_000,
                difficulty: U256::from(1000),
                gas_limit: U256::from(30_000_000),
                base_price_core: U256::from(1_000_000_000u64),
                base_price_espace: U256::zero(),
                height: 1000,
                blame: 0,
                finalized_epoch: 0,
                base_reward: U256::from(1),
                transactions: vec![tx],
                transaction_refs: Vec::new(),
                pos_rewards: Vec::new(),
                unlock_events: Vec::new(),
                pos_view: None,
            }],
        };

        let packet = encode_packet(&raw).expect("encode tx packet");
        let report = verify_packet(&packet).expect("verify tx packet");
        assert_eq!(report.block_count, 1);
        assert_eq!(report.transaction_blocks, 1);
        assert_eq!(report.transaction_items, 1);

        let decoded = decode_packet(&packet).expect("decode tx packet");
        assert_eq!(decoded.blocks[0].transactions.len(), 1);
        // The signed epoch_height offset (900 - 1000 = -100) must be recovered
        // exactly, not flipped to 1100 by an unsigned (abs_diff) decode.
        match &decoded.blocks[0].transactions[0].transaction.unsigned {
            primitives::transaction::Transaction::Native(native) => {
                assert_eq!(*native.epoch_height(), 900);
            }
            primitives::transaction::Transaction::Ethereum(_) => panic!("expected native tx"),
        }
        let reencoded = encode_packet(&decoded).expect("reencode tx packet");
        assert_eq!(reencoded, packet);
        let reencoded_report = verify_packet(&reencoded).expect("verify reencoded tx packet");
        assert_eq!(reencoded_report.transaction_blocks, 1);
        assert_eq!(reencoded_report.transaction_items, 1);
    }

    #[test]
    fn raw_to_packet_roundtrip_with_unlock_events() {
        let author = Address::from_low_u64_be(1);
        let node_id = H256::from_low_u64_be(0xABCD);
        let raw = Packet {
            prev_last_hash: H256::from_low_u64_be(9),
            prev_last_deferred_state_root: H256::from_low_u64_be(10),
            first_block_number: 100,
            min_timestamp: 1_700_000_000,
            min_height: 42,
            min_pos_height: 0,
            addresses: vec![author],
            pos_entries: Vec::new(),
            difficulties: vec![U256::from(1000)],
            sender_base_nonces: Vec::new(),
            gas_prices: Vec::new(),
            blocks: vec![Block {
                epoch: 42,
                index: 0,
                hash: H256::from_low_u64_be(11),
                deferred_state_root: H256::from_low_u64_be(12),
                deferred_receipts_root: H256::from_low_u64_be(13),
                deferred_logs_bloom_hash: H256::from_low_u64_be(14),
                flags: FLAG_PIVOT,
                author,
                timestamp: 1_700_000_000,
                difficulty: U256::from(1000),
                gas_limit: U256::from(30_000_000),
                base_price_core: U256::zero(),
                base_price_espace: U256::zero(),
                height: 42,
                blame: 0,
                finalized_epoch: 0,
                base_reward: U256::from(1),
                transactions: Vec::new(),
                transaction_refs: Vec::new(),
                pos_rewards: Vec::new(),
                unlock_events: vec![
                    UnlockEntry { identifier: node_id, unlocked: 100 },
                    UnlockEntry { identifier: H256::from_low_u64_be(0xBEEF), unlocked: 50 },
                ],
                pos_view: None,
            }],
        };

        let packet = encode_packet(&raw).expect("encode unlock packet");
        let report = verify_packet(&packet).expect("verify unlock packet");
        assert_eq!(report.block_count, 1);
        assert_eq!(report.transaction_blocks, 1);

        let decoded = decode_packet(&packet).expect("decode unlock packet");
        assert_eq!(decoded.blocks[0].transactions.len(), 0);
        assert_eq!(decoded.blocks[0].unlock_events.len(), 2);
        assert_eq!(decoded.blocks[0].unlock_events[0].identifier, node_id);
        assert_eq!(decoded.blocks[0].unlock_events[0].unlocked, 100);
        assert_eq!(decoded.blocks[0].unlock_events[1].unlocked, 50);

        let reencoded = encode_packet(&decoded).expect("reencode unlock packet");
        assert_eq!(reencoded, packet);
    }
}
