#![no_main]

use cfx_replay_data_extractor::validate::validate_replay_packet;
use cfxpack::{
    decode::decode_packet,
    packet::{encode_packet, Block, Packet, FLAG_PIVOT},
    verify::verify_packet,
};
use cfx_types::{Address, AddressSpaceUtil, H256, U256};
use libfuzzer_sys::fuzz_target;
use primitives::{
    transaction::{
        Cip1559Transaction, Cip2930Transaction, NativeTransaction,
        TypedNativeTransaction,
    },
    Action, SignedTransaction,
};

fuzz_target!(|data: &[u8]| {
    let Some(raw) = raw_from_bytes(data) else {
        return;
    };
    let Ok(packet) = encode_packet(&raw) else {
        return;
    };
    let Ok(decoded) = decode_packet(&packet) else {
        panic!("decode failed for encoded packet");
    };
    let Ok(reencoded) = encode_packet(&decoded) else {
        panic!("reencode failed for decoded packet");
    };
    assert_eq!(packet, reencoded);
    let first = verify_packet(&packet).expect("encoded packet should verify");
    let second = verify_packet(&reencoded).expect("reencoded packet should verify");
    assert_eq!(first, second);
    let first_replay = validate_replay_packet(&packet).expect("encoded packet should replay");
    let second_replay = validate_replay_packet(&reencoded).expect("reencoded packet should replay");
    assert_eq!(first_replay, second_replay);
});

fn raw_from_bytes(data: &[u8]) -> Option<Packet> {
    if data.len() < 8 {
        return None;
    }
    let block_count = (data[0] as usize % 8) + 1;
    let author_count = ((data[1] as usize % 4) + 3).min(8);
    let mut addresses = Vec::new();
    for i in 0..author_count {
        addresses.push(Address::from_low_u64_be(i as u64 + 1));
    }
    let with_txs = data.get(2).copied().unwrap_or(0) & 1 == 1;
    let duplicate_txs = data.get(3).copied().unwrap_or(0) & 1 == 1;

    let mut blocks = Vec::new();
    let mut first_tx: Option<SignedTransaction> = None;
    let mut cursor = 4usize;
    let epoch = 1_000 + block_count as u64 - 1;
    for i in 0..block_count {
        let author = addresses[i % addresses.len()];
        let height = 1_000 + i as u64;
        let timestamp = 1_700_000_000 + next_u16(data, &mut cursor) as u64;
        let mut transactions = Vec::new();
        // Real blocks never carry two transactions with the same hash (distinct
        // signed txs hash differently, and a tx is packed at most once per
        // block), so enforce per-block hash uniqueness here. The dedup that the
        // codec performs is *cross-block* tx reuse (legal in Conflux, exercised
        // by `duplicate_txs` below) — an intra-block self-reference, which only
        // arises from `fake_sign` hash collisions, is not a real input.
        let mut block_tx_hashes = std::collections::HashSet::new();
        if with_txs && (data.get(cursor).copied().unwrap_or(0) as usize + i) % 3 != 0 {
            let tx_count = (data.get(cursor).copied().unwrap_or(0) as usize % 3) + 1;
            cursor = cursor.saturating_add(1);
            for tx_index in 0..tx_count {
                if duplicate_txs && i > 0 && tx_index == 0 {
                    if let Some(tx) = &first_tx {
                        if block_tx_hashes.insert(tx.hash()) {
                            transactions.push(tx.clone());
                        }
                        continue;
                    }
                }
                let tx = make_tx(
                    data,
                    &mut cursor,
                    epoch,
                    addresses[(tx_index + 1) % addresses.len()],
                    addresses[(tx_index + 2) % addresses.len()],
                );
                if first_tx.is_none() {
                    first_tx = Some(tx.clone());
                }
                if block_tx_hashes.insert(tx.hash()) {
                    transactions.push(tx);
                }
            }
        }
        blocks.push(Block {
            epoch,
            index: i,
            hash: H256::from_low_u64_be(10_000 + i as u64),
            deferred_state_root: prefixed_hash(20_000 + i as u64),
            deferred_receipts_root: prefixed_hash(30_000 + i as u64),
            deferred_logs_bloom_hash: prefixed_hash(40_000 + i as u64),
            flags: if i + 1 == block_count { FLAG_PIVOT } else { 0 },
            author,
            timestamp,
            difficulty: U256::from((next_u16(data, &mut cursor) as u64) + 1),
            gas_limit: if data.get(cursor).copied().unwrap_or(0) & 1 == 0 {
                U256::from(30_000_000)
            } else {
                U256::from(60_000_000)
            },
            base_price_core: U256::from(1_000_000_000u64),
            base_price_espace: U256::zero(),
            height,
            blame: (data.get(cursor).copied().unwrap_or(0) % 4) as u64,
            finalized_epoch: 0,
            base_reward: U256::from(next_u16(data, &mut cursor) as u64),
            transactions,
            transaction_refs: Vec::new(),
        });
        cursor = cursor.saturating_add(1);
    }
    let min_timestamp = blocks.iter().map(|b| b.timestamp).min()?;
    let min_height = blocks.iter().map(|b| b.height).min()?;
    let mut difficulties = Vec::new();
    for block in &blocks {
        if !difficulties.contains(&block.difficulty) {
            difficulties.push(block.difficulty);
        }
    }
    Some(Packet {
        prev_last_hash: H256::from_low_u64_be(1),
        prev_last_deferred_state_root: H256::from_low_u64_be(2),
        first_block_number: 1_000,
        min_timestamp,
        min_height,
        min_pos_height: 0,
        addresses,
        pos_entries: Vec::new(),
        difficulties,
        sender_base_nonces: Vec::new(),
        gas_prices: vec![U256::from(1_000_000_000u64)],
        blocks,
    })
}

fn make_tx(
    data: &[u8], cursor: &mut usize, epoch_height: u64, sender: Address,
    receiver: Address,
) -> SignedTransaction {
    let nonce = U256::from(next_u16(data, cursor) as u64);
    let gas_price = if data.get(*cursor).copied().unwrap_or(0) & 1 == 0 {
        U256::from(1_000_000_000u64)
    } else {
        U256::from((next_u16(data, cursor) as u64) + 1)
    };
    let gas = U256::from(21_000 + next_u16(data, cursor) as u64);
    let action = if data.get(*cursor).copied().unwrap_or(0) & 1 == 0 {
        Action::Call(receiver)
    } else {
        Action::Create
    };
    *cursor = cursor.saturating_add(1);
    let data_len = (data.get(*cursor).copied().unwrap_or(0) % 24) as usize;
    *cursor = cursor.saturating_add(1);
    let mut payload = Vec::new();
    for _ in 0..data_len {
        payload.push(data.get(*cursor).copied().unwrap_or(0));
        *cursor = cursor.saturating_add(1);
    }
    match data.get(*cursor).copied().unwrap_or(0) % 3 {
        0 => NativeTransaction {
            nonce,
            gas_price,
            gas,
            action,
            value: U256::from(next_u16(data, cursor) as u64),
            storage_limit: next_u16(data, cursor) as u64,
            epoch_height,
            chain_id: 1029,
            data: payload.into(),
        }
        .fake_sign(sender.with_native_space()),
        1 => TypedNativeTransaction::Cip2930(Cip2930Transaction {
            nonce,
            gas_price,
            gas,
            action,
            value: U256::from(next_u16(data, cursor) as u64),
            storage_limit: next_u16(data, cursor) as u64,
            epoch_height,
            chain_id: 1029,
            data: payload.into(),
            access_list: Vec::new(),
        })
        .fake_sign_rpc(sender.with_native_space()),
        _ => TypedNativeTransaction::Cip1559(Cip1559Transaction {
            nonce,
            max_priority_fee_per_gas: gas_price,
            max_fee_per_gas: gas_price,
            gas,
            action,
            value: U256::from(next_u16(data, cursor) as u64),
            storage_limit: next_u16(data, cursor) as u64,
            epoch_height,
            chain_id: 1029,
            data: payload.into(),
            access_list: Vec::new(),
        })
        .fake_sign_rpc(sender.with_native_space()),
    }
}

fn next_u16(data: &[u8], cursor: &mut usize) -> u16 {
    let lo = data.get(*cursor).copied().unwrap_or(0) as u16;
    let hi = data.get(cursor.saturating_add(1)).copied().unwrap_or(0) as u16;
    *cursor = cursor.saturating_add(2);
    lo | (hi << 8)
}

fn prefixed_hash(value: u64) -> H256 {
    let mut bytes = [0u8; 32];
    bytes[..8].copy_from_slice(&value.to_be_bytes());
    H256::from(bytes)
}
