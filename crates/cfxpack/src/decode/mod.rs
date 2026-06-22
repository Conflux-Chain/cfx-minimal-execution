//! `.cfxpack` decoder: parse a packet back into a [`Packet`]. The header,
//! interned tables, and fixed-size block prefix records are decoded here; the
//! transaction segment is decoded in [`tx`].

use crate::codec::{read_qc5e, read_qc8, read_u32_le, read_u64_le, read_uleb128};
use crate::packet::{
    Block, Packet, PosLookupEntry, SenderBaseNonce, FLAG_HAS_TRANSACTIONS, FLAG_PIVOT,
    HEADER_FIXED_LEN, HEADER_LEN, HEADER_OFFSET_COUNT,
};
use anyhow::{anyhow, ensure, Context, Result};
use cfx_types::{Address, H256, U256};
use primitives::SignedTransaction;

mod tx;
use tx::decode_tx_payload;

pub fn decode_packet(data: &[u8]) -> Result<Packet> {
    ensure!(data.len() >= HEADER_LEN, "packet shorter than header");
    let prev_last_hash = H256::from_slice(&data[0..32]);
    let prev_last_deferred_state_root = H256::from_slice(&data[32..64]);
    let first_block_number = read_u64_le(data, 64)?;
    let min_timestamp = read_u64_le(data, 72)?;
    let min_height = read_u64_le(data, 80)?;
    let min_pos_height = read_u32_le(data, 88)? as u64;
    let block_prefix_size = data[92] as usize;
    ensure!(
        matches!(block_prefix_size, 64 | 72 | 80 | 88 | 96),
        "invalid block_prefix_size"
    );

    let offsets = read_offsets(data)?;
    ensure_offsets(&offsets, data.len())?;

    let addresses = decode_addresses(&data[offsets[0]..offsets[1]])?;
    let pos_entries = decode_pos_entries(&data[offsets[1]..offsets[2]])?;
    let difficulties = decode_u256_table(&data[offsets[2]..offsets[3]])?;
    let sender_base_nonces = decode_sender_base_nonces(&data[offsets[3]..offsets[4]])?;
    let gas_prices = decode_u256_table(&data[offsets[4]..offsets[5]])?;

    let block_records =
        decode_block_records(data, offsets[5], offsets[6], offsets[7], block_prefix_size)?;
    let mut blocks = Vec::with_capacity(block_records.len());
    let mut decoded_txs = Vec::<Vec<SignedTransaction>>::new();
    for (index, record) in block_records.iter().enumerate() {
        let block = decode_block_record(
            record,
            index,
            min_timestamp,
            min_height,
            &addresses,
            &difficulties,
        )?;
        decoded_txs.push(Vec::new());
        blocks.push(block);
    }
    assign_epoch_from_pivots(&mut blocks)?;

    for (index, (record, block)) in block_records.iter().zip(blocks.iter_mut()).enumerate() {
        if block.flags & FLAG_HAS_TRANSACTIONS != 0 {
            let (transactions, transaction_refs) = decode_tx_payload(
                data,
                offsets[7],
                record.tx_offset_units,
                block.flags,
                block.epoch,
                &addresses,
                &gas_prices,
                &sender_base_nonces,
                &decoded_txs[..index],
            )?;
            block.transactions = transactions;
            block.transaction_refs = transaction_refs;
        }
        decoded_txs[index] = block.transactions.clone();
    }

    Ok(Packet {
        prev_last_hash,
        prev_last_deferred_state_root,
        first_block_number,
        min_timestamp,
        min_height,
        min_pos_height,
        addresses,
        pos_entries,
        difficulties,
        sender_base_nonces,
        gas_prices,
        blocks,
    })
}

struct DecodedRecord {
    bytes: Vec<u8>,
    tx_offset_units: u64,
}

impl std::ops::Deref for DecodedRecord {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.bytes
    }
}

fn decode_block_records(
    data: &[u8],
    block_header_offset: usize,
    block_body_offset: usize,
    tx_segment_offset: usize,
    prefix_size: usize,
) -> Result<Vec<DecodedRecord>> {
    let block_count = read_u32_le(data, block_header_offset)? as usize;
    let bitmap_len = block_count.div_ceil(8);
    let bitmap = &data[block_header_offset + 4..block_header_offset + 4 + bitmap_len];
    let prefix_total = block_count
        .checked_mul(prefix_size)
        .context("block prefix size overflow")?;
    ensure!(
        block_body_offset + prefix_total <= tx_segment_offset,
        "block prefix area exceeds tx segment"
    );
    let mut overflow_offset = block_body_offset + prefix_total;
    let mut out = Vec::with_capacity(block_count);
    for index in 0..block_count {
        let mut bytes = data[block_body_offset + index * prefix_size
            ..block_body_offset + (index + 1) * prefix_size]
            .to_vec();
        if bitmap[index / 8] & (1 << (index % 8)) != 0 {
            let len = read_uleb128(data, &mut overflow_offset)? as usize;
            ensure!(
                overflow_offset + len <= tx_segment_offset,
                "block overflow exceeds tx segment"
            );
            bytes.extend_from_slice(&data[overflow_offset..overflow_offset + len]);
            overflow_offset += len;
        }
        let tx_offset_units = peek_tx_offset_units(&bytes)?;
        out.push(DecodedRecord {
            bytes,
            tx_offset_units,
        });
    }
    Ok(out)
}

fn decode_block_record(
    record: &[u8],
    index: usize,
    min_timestamp: u64,
    min_height: u64,
    addresses: &[Address],
    difficulties: &[U256],
) -> Result<Block> {
    ensure!(record.len() >= 45, "block record too short");
    let hash = H256::from_slice(&record[0..32]);
    let deferred_state_root = h256_prefix(&record[32..36]);
    let deferred_receipts_root = h256_prefix(&record[36..40]);
    let deferred_logs_bloom_hash = h256_prefix(&record[40..44]);
    let flags = record[44];
    let mut offset = 45;
    let author = table_get(addresses, read_uleb128(record, &mut offset)? as usize)?;
    let timestamp = min_timestamp + read_uleb128(record, &mut offset)?;
    let difficulty = table_get(difficulties, read_uleb128(record, &mut offset)? as usize)?;
    let gas_limit = read_qc5e(record, &mut offset)?.gas_limit();
    let base_price_core = read_qc5e(record, &mut offset)?.base_price(false);
    let base_price_espace = read_qc5e(record, &mut offset)?.base_price(true);
    let height = min_height + read_uleb128(record, &mut offset)?;
    let blame = read_uleb128(record, &mut offset)?;
    let finalized_epoch = read_uleb128(record, &mut offset)?;
    let _tx_segment_offset = read_uleb128(record, &mut offset)?;
    let base_reward = read_qc8(record, &mut offset)?;
    Ok(Block {
        epoch: 0,
        index,
        hash,
        deferred_state_root,
        deferred_receipts_root,
        deferred_logs_bloom_hash,
        flags,
        author,
        timestamp,
        difficulty,
        gas_limit,
        base_price_core,
        base_price_espace,
        height,
        blame,
        finalized_epoch,
        base_reward,
        transactions: Vec::new(),
        transaction_refs: Vec::new(),
    })
}

fn assign_epoch_from_pivots(blocks: &mut [Block]) -> Result<()> {
    let mut group_start = 0usize;
    while group_start < blocks.len() {
        let Some(relative_pivot) = blocks[group_start..]
            .iter()
            .position(|block| block.flags & FLAG_PIVOT != 0)
        else {
            return Err(anyhow!(
                "packet block list has no pivot for final epoch group"
            ));
        };
        let pivot_index = group_start + relative_pivot;
        let epoch = blocks[pivot_index].height;
        for block in &mut blocks[group_start..=pivot_index] {
            block.epoch = epoch;
        }
        group_start = pivot_index + 1;
    }
    Ok(())
}

fn peek_tx_offset_units(record: &[u8]) -> Result<u64> {
    let mut offset = 45;
    for _ in 0..3 {
        read_uleb128(record, &mut offset)?;
    }
    for _ in 0..3 {
        read_qc5e(record, &mut offset)?;
    }
    for _ in 0..3 {
        read_uleb128(record, &mut offset)?;
    }
    read_uleb128(record, &mut offset)
}

fn decode_addresses(data: &[u8]) -> Result<Vec<Address>> {
    ensure!(data.len() % 20 == 0, "address table has trailing bytes");
    Ok(data.chunks_exact(20).map(Address::from_slice).collect())
}

fn decode_pos_entries(data: &[u8]) -> Result<Vec<PosLookupEntry>> {
    ensure!(data.len() % 34 == 0, "PoS table has trailing bytes");
    Ok(data
        .chunks_exact(34)
        .map(|chunk| PosLookupEntry {
            hash: H256::from_slice(&chunk[..32]),
            height_offset: u16::from_le_bytes([chunk[32], chunk[33]]),
        })
        .collect())
}

fn decode_u256_table(data: &[u8]) -> Result<Vec<U256>> {
    let data = trim_zero_padding(data, 32)?;
    ensure!(data.len() % 32 == 0, "U256 table has trailing bytes");
    Ok(data.chunks_exact(32).map(U256::from_big_endian).collect())
}

fn decode_sender_base_nonces(data: &[u8]) -> Result<Vec<SenderBaseNonce>> {
    let mut offset = 0;
    let mut out = Vec::new();
    while offset < data.len() {
        out.push(SenderBaseNonce {
            sender_index: read_uleb128(data, &mut offset)? as usize,
            base_nonce: read_uleb128(data, &mut offset)?,
        });
    }
    Ok(out)
}

pub(super) fn decode_u256_bytes(bytes: Vec<u8>) -> Result<U256> {
    ensure!(bytes.len() <= 32, "U256 byte value exceeds 32 bytes");
    Ok(U256::from_big_endian(&bytes))
}

pub(super) fn table_get<T: Copy>(table: &[T], index: usize) -> Result<T> {
    table
        .get(index)
        .copied()
        .ok_or_else(|| anyhow!("lookup table index {index} out of bounds"))
}

fn h256_prefix(prefix: &[u8]) -> H256 {
    let mut bytes = [0u8; 32];
    bytes[..prefix.len()].copy_from_slice(prefix);
    H256::from(bytes)
}

/// Return, for every block in the packet, the absolute byte offset of its
/// 1-byte `flags` field together with the block hash. Both the hash (record
/// bytes 0..32) and the flags byte (record byte 44) live in the fixed-size
/// prefix area (`block_prefix_size` is at least 64), so they can be located
/// without decoding the variable-length record body or the transaction segment.
/// Used by the in-place flag patcher.
pub fn block_flag_sites(data: &[u8]) -> Result<Vec<(usize, H256)>> {
    ensure!(data.len() >= HEADER_LEN, "packet shorter than header");
    let block_prefix_size = data[92] as usize;
    ensure!(
        matches!(block_prefix_size, 64 | 72 | 80 | 88 | 96),
        "invalid block_prefix_size"
    );
    let offsets = read_offsets(data)?;
    ensure_offsets(&offsets, data.len())?;
    let block_header_offset = offsets[5];
    let block_body_offset = offsets[6];
    let tx_segment_offset = offsets[7];
    let block_count = read_u32_le(data, block_header_offset)? as usize;
    let prefix_total = block_count
        .checked_mul(block_prefix_size)
        .context("block prefix size overflow")?;
    ensure!(
        block_body_offset + prefix_total <= tx_segment_offset,
        "block prefix area exceeds tx segment"
    );
    let mut sites = Vec::with_capacity(block_count);
    for index in 0..block_count {
        let record = block_body_offset + index * block_prefix_size;
        let hash = H256::from_slice(&data[record..record + 32]);
        sites.push((record + 44, hash));
    }
    Ok(sites)
}

fn read_offsets(data: &[u8]) -> Result<[usize; HEADER_OFFSET_COUNT]> {
    let mut offsets = [0usize; HEADER_OFFSET_COUNT];
    for (i, item) in offsets.iter_mut().enumerate() {
        *item = read_u32_le(data, HEADER_FIXED_LEN + i * 4)? as usize;
    }
    Ok(offsets)
}

fn ensure_offsets(offsets: &[usize; HEADER_OFFSET_COUNT], len: usize) -> Result<()> {
    let mut previous = HEADER_LEN;
    for offset in offsets {
        ensure!(*offset >= previous, "offset table is not monotonic");
        ensure!(*offset <= len, "offset exceeds packet length");
        previous = *offset;
    }
    Ok(())
}

fn trim_zero_padding(data: &[u8], alignment: usize) -> Result<&[u8]> {
    let trailing = data.len() % alignment;
    if trailing == 0 {
        return Ok(data);
    }
    let split = data.len() - trailing;
    ensure!(
        data[split..].iter().all(|byte| *byte == 0),
        "non-zero table padding"
    );
    Ok(&data[..split])
}
