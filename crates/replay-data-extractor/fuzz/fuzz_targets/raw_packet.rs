#![no_main]

use cfx_replay_data_extractor::{
    packet::{BlockInput, PacketInput, FLAG_ADAPTIVE, FLAG_ESPACE, FLAG_PIVOT},
    raw::encode_raw_data,
    validate::validate_replay_packet,
    verify::verify_packet,
};
use cfx_types::{Address, H256, U256};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() < 8 {
        return;
    }
    let author = Address::from_low_u64_be(u64::from(data[0]) + 1);
    let difficulty = U256::from(u64::from(data[1]) + 1);
    let block_count = usize::from(data[2] % 5) + 1;
    let min_height = u64::from(data[3]);
    let epoch = min_height + block_count as u64 - 1;
    let min_timestamp = 1_700_000_000 + u64::from(data[4]);
    let mut blocks = Vec::with_capacity(block_count);
    for i in 0..block_count {
        let b = data.get(5 + i).copied().unwrap_or(0);
        blocks.push(BlockInput {
            epoch,
            index: i,
            hash: H256::from_low_u64_be(100 + i as u64),
            deferred_state_root: H256::from_low_u64_be(200 + i as u64),
            deferred_receipts_root: H256::from_low_u64_be(300 + i as u64),
            deferred_logs_bloom_hash: H256::from_low_u64_be(400 + i as u64),
            flags: (if b & 1 != 0 { FLAG_ADAPTIVE } else { 0 })
                | (if i + 1 == block_count { FLAG_PIVOT | FLAG_ESPACE } else { 0 }),
            author,
            timestamp: min_timestamp + i as u64,
            difficulty,
            gas_limit: if b & 2 == 0 { U256::from(30_000_000) } else { U256::from(60_000_000) },
            base_price_core: if b & 4 == 0 { U256::zero() } else { U256::from(1_000_000_000) },
            base_price_espace: if b & 8 == 0 { U256::zero() } else { U256::from(20_000_000_000u64) },
            height: min_height + i as u64,
            blame: u64::from(b >> 4),
            finalized_epoch: u64::from(b % 3),
            base_reward: U256::from(u64::from(b) + 1),
            transactions: Vec::new(),
            transaction_refs: Vec::new(),
        });
    }

    let raw = PacketInput {
        prev_last_hash: H256::from_low_u64_be(1),
        prev_last_deferred_state_root: H256::from_low_u64_be(2),
        first_block_number: 1,
        min_timestamp,
        min_height,
        min_pos_height: 0,
        addresses: vec![author],
        pos_entries: Vec::new(),
        difficulties: vec![difficulty],
        sender_base_nonces: Vec::new(),
        gas_prices: Vec::new(),
        blocks,
    };
    if let Ok(packet) = encode_raw_data(&raw) {
        let verify = verify_packet(&packet).unwrap();
        let replay = validate_replay_packet(&packet).unwrap();
        assert_eq!(verify.block_count as usize, replay.block_count);
        assert_eq!(verify.transaction_items as usize, replay.transaction_count);
    }
});
