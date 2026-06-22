//! Coverage for `validate_replay_packet` — the extractor-only replay-plan
//! summary. These assertions used to live in `packet.rs`'s unit tests, but that
//! module is now shared verbatim with the executor (which has no `validate`
//! module), so the plan checks moved here where they stay extractor-local.

use cfx_replay_data_extractor::validate::validate_replay_packet;
use cfx_types::{Address, AddressSpaceUtil, H256, U256};
use cfxpack::packet::{encode_packet, Block, Packet, FLAG_ESPACE, FLAG_PIVOT};
use primitives::{transaction::NativeTransaction, Action};

fn block(index: usize, height: u64, author: Address, flags: u8) -> Block {
    Block {
        epoch: 45,
        index,
        hash: H256::from_low_u64_be(11 + index as u64),
        deferred_state_root: H256::from_low_u64_be(12),
        deferred_receipts_root: H256::from_low_u64_be(13),
        deferred_logs_bloom_hash: H256::from_low_u64_be(14),
        flags,
        author,
        timestamp: 1_700_000_000,
        difficulty: U256::from(1000),
        gas_limit: U256::from(30_000_000),
        base_price_core: U256::zero(),
        base_price_espace: U256::zero(),
        height,
        blame: 0,
        finalized_epoch: 0,
        base_reward: U256::from(1),
        transactions: Vec::new(),
        transaction_refs: Vec::new(),
    }
}

#[test]
fn replay_plan_counts_minimal_blocks() {
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
            block(0, 42, author, 0),
            block(1, 45, author, FLAG_PIVOT | FLAG_ESPACE),
        ],
    };

    let packet = encode_packet(&raw).expect("encode raw packet");
    let replay = validate_replay_packet(&packet).expect("validate replay plan");
    assert_eq!(replay.epoch_count, 1);
    assert_eq!(replay.block_count, 2);
    assert_eq!(replay.transaction_count, 0);
}

#[test]
fn replay_plan_counts_native_transaction() {
    let author = Address::from_low_u64_be(1);
    let sender = Address::from_low_u64_be(2);
    let receiver = Address::from_low_u64_be(3);
    let tx = NativeTransaction {
        nonce: U256::from(7),
        gas_price: U256::from(1_000_000_000u64),
        gas: U256::from(21_000),
        action: Action::Call(receiver),
        value: U256::from(42),
        storage_limit: 0,
        epoch_height: 1000,
        chain_id: 1029,
        data: Vec::new().into(),
    }
    .fake_sign(sender.with_native_space());

    let mut pivot = block(0, 1000, author, FLAG_PIVOT);
    pivot.epoch = 1000;
    pivot.base_price_core = U256::from(1_000_000_000u64);
    pivot.transactions = vec![tx];

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
        blocks: vec![pivot],
    };

    let packet = encode_packet(&raw).expect("encode tx packet");
    let replay = validate_replay_packet(&packet).expect("validate tx replay plan");
    assert_eq!(replay.epoch_count, 1);
    assert_eq!(replay.transaction_count, 1);
    assert_eq!(replay.native_transaction_count, 1);
    assert_eq!(replay.espace_transaction_count, 0);
}
