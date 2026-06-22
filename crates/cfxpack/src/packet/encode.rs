//! `.cfxpack` encoder: lower a [`Packet`] into the packet byte layout —
//! interned tables, fixed-size block prefix records with an overflow area, and
//! a compressed transaction segment.

use super::{
    Block, Packet, FLAG_HAS_TRANSACTIONS, FLAG_PIVOT, FLAG_TX_COMPRESSED, HEADER_FIXED_LEN,
    HEADER_LEN, HEADER_OFFSET_COUNT, PACKET_EPOCHS,
};
use crate::codec::{
    align_to, put_qc5e, put_qc8, put_u32_le, put_uleb128, qc5e_base_price_enum,
    qc5e_gas_limit_enum, u256_to_be,
};
use anyhow::{anyhow, ensure, Context, Result};
use cfx_types::{Address, H256, U256};
use primitives::{Action, SignedTransaction, Transaction};
use rlp::RlpStream;
use snap::raw::Encoder as SnapEncoder;
use std::collections::HashMap;

pub fn encode_packet(input: &Packet) -> Result<Vec<u8>> {
    validate_input(input)?;

    let address_index = index_addresses(&input.addresses);
    let difficulty_index = index_u256(&input.difficulties);
    let gas_price_index = index_u256(&input.gas_prices);
    let sender_nonce_index = input
        .sender_base_nonces
        .iter()
        .map(|x| (x.sender_index, x.base_nonce))
        .collect::<HashMap<_, _>>();

    let (block_records, tx_segment) = encode_blocks(
        input,
        &address_index,
        &gas_price_index,
        &sender_nonce_index,
        &difficulty_index,
    )?;
    let prefix_size = choose_prefix_size(&block_records);
    let (block_body, extension_bitmap) = pack_block_body(&block_records, prefix_size);
    assemble_packet(
        input,
        prefix_size,
        block_records.len(),
        &extension_bitmap,
        &block_body,
        &tx_segment,
    )
}

/// Encode every block into its fixed-size prefix record and append its
/// (optionally snappy-compressed) transaction payload to the shared tx segment.
fn encode_blocks(
    input: &Packet,
    address_index: &HashMap<Address, usize>,
    gas_price_index: &HashMap<U256, usize>,
    sender_nonce_index: &HashMap<usize, u64>,
    difficulty_index: &HashMap<U256, usize>,
) -> Result<(Vec<Vec<u8>>, Vec<u8>)> {
    let mut tx_payloads = Vec::with_capacity(input.blocks.len());
    let mut seen_txs = HashMap::<H256, (usize, usize)>::new();
    for block in &input.blocks {
        tx_payloads.push(encode_tx_payload(
            block,
            address_index,
            gas_price_index,
            sender_nonce_index,
            &mut seen_txs,
        )?);
    }

    let mut block_records = Vec::with_capacity(input.blocks.len());
    let mut tx_segment = Vec::new();
    for (block, tx_payload) in input.blocks.iter().zip(tx_payloads.iter()) {
        let mut flags = block.flags & !(FLAG_HAS_TRANSACTIONS | FLAG_TX_COMPRESSED);
        let tx_segment_offset = if tx_payload.is_empty() {
            0
        } else {
            flags |= FLAG_HAS_TRANSACTIONS;
            align_to(&mut tx_segment, 4);
            let offset = tx_segment.len() / 4;
            let payload = if tx_payload.len() > 512 {
                flags |= FLAG_TX_COMPRESSED;
                SnapEncoder::new()
                    .compress_vec(tx_payload)
                    .context("snappy-compress tx payload")?
            } else {
                tx_payload.clone()
            };
            put_uleb128(&mut tx_segment, payload.len() as u64);
            tx_segment.extend_from_slice(&payload);
            offset as u64
        };
        block_records.push(encode_block_record(
            block,
            flags,
            tx_segment_offset,
            input,
            address_index,
            difficulty_index,
        )?);
    }
    Ok((block_records, tx_segment))
}

/// Lay the block records into the fixed-size prefix area, spilling the bytes
/// past `prefix_size` into an overflow tail and recording which records
/// overflowed in the returned extension bitmap.
fn pack_block_body(block_records: &[Vec<u8>], prefix_size: usize) -> (Vec<u8>, Vec<u8>) {
    let block_count = block_records.len();
    let mut extension_bitmap = vec![0u8; (block_count + 7) / 8];
    let mut block_body = Vec::with_capacity(block_count * prefix_size);
    let mut overflow = Vec::new();
    for (i, record) in block_records.iter().enumerate() {
        if record.len() > prefix_size {
            extension_bitmap[i / 8] |= 1 << (i % 8);
        }
        let prefix_len = record.len().min(prefix_size);
        block_body.extend_from_slice(&record[..prefix_len]);
        block_body.resize(block_body.len() + (prefix_size - prefix_len), 0);
        if record.len() > prefix_size {
            let rest = &record[prefix_size..];
            put_uleb128(&mut overflow, rest.len() as u64);
            overflow.extend_from_slice(rest);
        }
    }
    block_body.extend_from_slice(&overflow);
    (block_body, extension_bitmap)
}

/// Write the fixed header, the interned tables, the block body, and the tx
/// segment into the final buffer, then backfill the section offset table.
fn assemble_packet(
    input: &Packet,
    prefix_size: usize,
    block_count: usize,
    extension_bitmap: &[u8],
    block_body: &[u8],
    tx_segment: &[u8],
) -> Result<Vec<u8>> {
    let mut out = vec![0u8; HEADER_LEN];
    out[0..32].copy_from_slice(input.prev_last_hash.as_bytes());
    out[32..64].copy_from_slice(input.prev_last_deferred_state_root.as_bytes());
    out[64..72].copy_from_slice(&input.first_block_number.to_le_bytes());
    out[72..80].copy_from_slice(&input.min_timestamp.to_le_bytes());
    out[80..88].copy_from_slice(&input.min_height.to_le_bytes());
    out[88..92].copy_from_slice(&(input.min_pos_height as u32).to_le_bytes());
    out[92] = prefix_size as u8;

    let mut offsets = Vec::new();
    offsets.push(out.len());
    for address in &input.addresses {
        out.extend_from_slice(address.as_bytes());
    }
    offsets.push(out.len());
    for entry in &input.pos_entries {
        out.extend_from_slice(entry.hash.as_bytes());
        out.extend_from_slice(&entry.height_offset.to_le_bytes());
    }
    offsets.push(out.len());
    for difficulty in &input.difficulties {
        out.extend_from_slice(&u256_to_be(*difficulty));
    }
    offsets.push(out.len());
    for entry in &input.sender_base_nonces {
        put_uleb128(&mut out, entry.sender_index as u64);
        put_uleb128(&mut out, entry.base_nonce);
    }
    offsets.push(out.len());
    for gas_price in &input.gas_prices {
        out.extend_from_slice(&u256_to_be(*gas_price));
    }
    align_to(&mut out, 32);
    offsets.push(out.len());
    put_u32_le(&mut out, block_count as u32);
    out.extend_from_slice(extension_bitmap);
    align_to(&mut out, 32);
    offsets.push(out.len());
    out.extend_from_slice(block_body);
    align_to(&mut out, 64);
    offsets.push(out.len());
    out.extend_from_slice(tx_segment);

    ensure!(
        offsets.len() == HEADER_OFFSET_COUNT,
        "internal offset count mismatch"
    );
    for (i, offset) in offsets.into_iter().enumerate() {
        let offset = u32::try_from(offset).context("packet offset exceeds u32")?;
        out[HEADER_FIXED_LEN + i * 4..HEADER_FIXED_LEN + i * 4 + 4]
            .copy_from_slice(&offset.to_le_bytes());
    }
    Ok(out)
}

fn validate_input(input: &Packet) -> Result<()> {
    ensure!(!input.blocks.is_empty(), "packet has no blocks");
    ensure!(
        matches!(input.blocks.first().map(|b| b.epoch), Some(e) if e + PACKET_EPOCHS >= e),
        "invalid epoch range"
    );
    let mut group_start = 0usize;
    let mut expected_index = 0usize;
    while group_start < input.blocks.len() {
        let Some(relative_pivot) = input.blocks[group_start..]
            .iter()
            .position(|block| block.flags & FLAG_PIVOT != 0)
        else {
            anyhow::bail!("packet input epoch group has no pivot block");
        };
        let pivot_index = group_start + relative_pivot;
        let epoch = input.blocks[pivot_index].height;
        for block in &input.blocks[group_start..=pivot_index] {
            ensure!(
                block.index == expected_index,
                "block index is not sequential"
            );
            ensure!(
                block.flags & 0b1000_0000 == 0,
                "reserved block flag bit (bit 7) must be zero"
            );
            ensure!(
                block.epoch == epoch,
                "block epoch must match its pivot block height"
            );
            expected_index += 1;
        }
        group_start = pivot_index + 1;
    }
    Ok(())
}

fn encode_block_record(
    block: &Block,
    flags: u8,
    tx_segment_offset: u64,
    input: &Packet,
    address_index: &HashMap<Address, usize>,
    difficulty_index: &HashMap<U256, usize>,
) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    out.extend_from_slice(block.hash.as_bytes());
    out.extend_from_slice(&block.deferred_state_root.as_bytes()[..4]);
    out.extend_from_slice(&block.deferred_receipts_root.as_bytes()[..4]);
    out.extend_from_slice(&block.deferred_logs_bloom_hash.as_bytes()[..4]);
    out.push(flags);
    put_uleb128(
        &mut out,
        *address_index
            .get(&block.author)
            .ok_or_else(|| anyhow!("missing author in address table"))? as u64,
    );
    put_uleb128(
        &mut out,
        block
            .timestamp
            .checked_sub(input.min_timestamp)
            .ok_or_else(|| anyhow!("timestamp below min_timestamp"))?,
    );
    put_uleb128(
        &mut out,
        *difficulty_index
            .get(&block.difficulty)
            .ok_or_else(|| anyhow!("missing difficulty in lookup table"))? as u64,
    );
    put_qc5e(
        &mut out,
        block.gas_limit,
        qc5e_gas_limit_enum(block.gas_limit),
    )?;
    put_qc5e(
        &mut out,
        block.base_price_core,
        qc5e_base_price_enum(block.base_price_core, false),
    )?;
    put_qc5e(
        &mut out,
        block.base_price_espace,
        qc5e_base_price_enum(block.base_price_espace, true),
    )?;
    put_uleb128(
        &mut out,
        block
            .height
            .checked_sub(input.min_height)
            .ok_or_else(|| anyhow!("height below min_height"))?,
    );
    put_uleb128(&mut out, block.blame);
    put_uleb128(&mut out, block.finalized_epoch);
    put_uleb128(&mut out, tx_segment_offset);
    put_qc8(&mut out, block.base_reward)?;
    Ok(out)
}

fn encode_tx_payload(
    block: &Block,
    address_index: &HashMap<Address, usize>,
    gas_price_index: &HashMap<U256, usize>,
    sender_nonce_index: &HashMap<usize, u64>,
    seen_txs: &mut HashMap<H256, (usize, usize)>,
) -> Result<Vec<u8>> {
    if block.transactions.is_empty() {
        return Ok(Vec::new());
    }
    let mut stream = RlpStream::new_list(block.transactions.len());
    let encoded_refs = (block.transaction_refs.len() == block.transactions.len())
        .then_some(block.transaction_refs.as_slice());
    for (tx_index, tx) in block.transactions.iter().enumerate() {
        let duplicate_ref = encoded_refs.and_then(|refs| refs[tx_index]).or_else(|| {
            if encoded_refs.is_none() {
                seen_txs.get(&tx.hash()).copied()
            } else {
                None
            }
        });
        if let Some((block_index, first_tx_index)) = duplicate_ref {
            stream.begin_list(3);
            stream.append(&1u8);
            stream.append(&(block_index as u64));
            stream.append(&(first_tx_index as u64));
            continue;
        }
        if encoded_refs.is_none() {
            seen_txs.insert(tx.hash(), (block.index, tx_index));
        }
        append_tx(
            &mut stream,
            block.epoch,
            tx,
            address_index,
            gas_price_index,
            sender_nonce_index,
        )?;
    }
    Ok(stream.out().to_vec())
}

fn append_tx(
    stream: &mut RlpStream,
    block_epoch: u64,
    tx: &SignedTransaction,
    address_index: &HashMap<Address, usize>,
    gas_price_index: &HashMap<U256, usize>,
    sender_nonce_index: &HashMap<usize, u64>,
) -> Result<()> {
    let sender_index = *address_index
        .get(&tx.sender)
        .ok_or_else(|| anyhow!("missing sender in address table"))?;
    let nonce = tx.nonce().low_u64();
    let encoded_nonce = sender_nonce_index
        .get(&sender_index)
        .map(|base| nonce.saturating_sub(*base))
        .unwrap_or(nonce);

    match &tx.transaction.unsigned {
        Transaction::Native(native) => {
            let epoch_height = *native.epoch_height();
            let has_access_list = tx.access_list().is_some();
            stream.begin_list(if has_access_list { 13 } else { 12 });
            stream.append(&0u8);
            stream.append(&(tx.type_id() as u64));
            stream.append(&(sender_index as u64));
            stream.append(&encoded_nonce);
            append_price(stream, *tx.gas_price(), gas_price_index);
            append_price(stream, *tx.max_priority_gas_price(), gas_price_index);
            stream.append(&tx.gas().low_u64());
            append_action(stream, tx.action(), address_index)?;
            stream.append(&u256_to_be(*tx.value()).as_slice());
            stream.append(native.storage_limit());
            stream.append(&epoch_height.abs_diff(block_epoch));
            let data: &[u8] = tx.data().as_ref();
            stream.append(&data);
            if let Some(list) = tx.access_list() {
                stream.append_list(list);
            }
        }
        Transaction::Ethereum(_) => {
            let has_access_list = tx.access_list().is_some();
            let has_auth = tx.authorization_list().is_some();
            stream.begin_list(10 + has_access_list as usize + has_auth as usize);
            stream.append(&2u8);
            stream.append(&(tx.type_id() as u64));
            stream.append(&(sender_index as u64));
            stream.append(&encoded_nonce);
            append_price(stream, *tx.gas_price(), gas_price_index);
            append_price(stream, *tx.max_priority_gas_price(), gas_price_index);
            stream.append(&tx.gas().low_u64());
            append_action(stream, tx.action(), address_index)?;
            stream.append(&u256_to_be(*tx.value()).as_slice());
            let data: &[u8] = tx.data().as_ref();
            stream.append(&data);
            if let Some(list) = tx.access_list() {
                stream.append_list(list);
            }
            if let Some(list) = tx.authorization_list() {
                stream.append_list(list);
            }
        }
    }
    Ok(())
}

fn append_action(
    stream: &mut RlpStream,
    action: Action,
    address_index: &HashMap<Address, usize>,
) -> Result<()> {
    match action {
        Action::Create => {
            stream.begin_list(1);
            stream.append(&0u8);
        }
        Action::Call(address) => {
            let index = *address_index
                .get(&address)
                .ok_or_else(|| anyhow!("missing action address in address table"))?;
            stream.begin_list(2);
            stream.append(&1u8);
            stream.append(&(index as u64));
        }
    }
    Ok(())
}

fn append_price(stream: &mut RlpStream, value: U256, gas_price_index: &HashMap<U256, usize>) {
    if let Some(index) = gas_price_index.get(&value) {
        stream.begin_list(2);
        stream.append(&0u8);
        stream.append(&(*index as u64));
    } else {
        stream.begin_list(2);
        stream.append(&1u8);
        stream.append(&u256_to_be(value).as_slice());
    }
}

fn choose_prefix_size(records: &[Vec<u8>]) -> usize {
    let mut n = 64;
    loop {
        let fit = records.iter().filter(|record| record.len() <= n).count();
        let p = fit as f64 / records.len() as f64;
        if n < 80 && p < 0.90 {
            n += 8;
        } else if n < 96 && n >= 80 && p < 0.70 {
            n += 8;
        } else {
            return n;
        }
    }
}

fn index_addresses(values: &[Address]) -> HashMap<Address, usize> {
    values
        .iter()
        .copied()
        .enumerate()
        .map(|(i, v)| (v, i))
        .collect()
}

fn index_u256(values: &[U256]) -> HashMap<U256, usize> {
    values
        .iter()
        .copied()
        .enumerate()
        .map(|(i, v)| (v, i))
        .collect()
}
