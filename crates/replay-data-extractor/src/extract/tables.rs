//! Deduplication / frequency tables that the packet format references by index.
//!
//! Each builder scans the collected [`RawBlock`]s once and produces one of the
//! shared lookup tables (addresses, gas prices, …) carried in the packet header.
//! Keeping them together isolates the "what gets interned and how it is ordered"
//! policy from the extraction orchestration in [`super`].

use super::{effective_pos_reference, RawBlock};
use anyhow::{anyhow, Context, Result};
use cfx_types::{Address, Space, H256, U256};
use cfxpack::packet::{PosLookupEntry, SenderBaseNonce};
use diem_types::committed_block::CommittedBlock;
use primitives::Action;
use std::collections::{HashMap, HashSet};

/// The full set of index tables carried by a packet header.
pub(super) struct Tables {
    pub addresses: Vec<Address>,
    pub pos_entries: Vec<PosLookupEntry>,
    pub difficulties: Vec<U256>,
    pub sender_base_nonces: Vec<SenderBaseNonce>,
    pub gas_prices: Vec<U256>,
}

/// Build every index table for a group of blocks in one pass-set.
pub(super) fn build_tables(
    blocks: &[RawBlock],
    pos_blocks: &HashMap<H256, CommittedBlock>,
    min_pos_height: u64,
    pos_reference_enable_height: u64,
) -> Result<Tables> {
    let addresses = build_address_table(blocks);
    let address_index = addresses
        .iter()
        .copied()
        .enumerate()
        .map(|(i, address)| (address, i))
        .collect::<HashMap<_, _>>();
    let difficulties = build_difficulty_table(blocks);
    let gas_prices = build_gas_price_table(blocks);
    let sender_base_nonces = build_sender_base_nonce_table(blocks, &address_index);
    let pos_entries = build_pos_lookup(
        blocks,
        pos_blocks,
        min_pos_height,
        pos_reference_enable_height,
    )?;
    Ok(Tables {
        addresses,
        pos_entries,
        difficulties,
        sender_base_nonces,
        gas_prices,
    })
}

fn build_address_table(blocks: &[RawBlock]) -> Vec<Address> {
    let mut stats = Frequency::<Address>::default();
    for block in blocks {
        stats.add(*block.header.author());
        for tx in &block.body {
            stats.add(tx.sender);
            if let Action::Call(address) = tx.action() {
                stats.add(address);
            }
        }
    }
    stats.into_sorted()
}

fn build_difficulty_table(blocks: &[RawBlock]) -> Vec<U256> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for block in blocks {
        let value = *block.header.difficulty();
        if seen.insert(value) {
            out.push(value);
        }
    }
    out
}

fn build_gas_price_table(blocks: &[RawBlock]) -> Vec<U256> {
    let mut stats = Frequency::<U256>::default();
    for block in blocks {
        if let Some(base) = block.header.base_price() {
            stats.add(*base.in_space(Space::Native));
            stats.add(*base.in_space(Space::Ethereum));
        }
        for tx in &block.body {
            stats.add(*tx.gas_price());
            stats.add(*tx.max_priority_gas_price());
        }
    }
    stats
        .into_sorted_with_counts()
        .into_iter()
        .filter(|(_, count)| *count > 3)
        .take(16)
        .map(|(value, _)| value)
        .collect()
}

fn build_sender_base_nonce_table(
    blocks: &[RawBlock],
    address_index: &HashMap<Address, usize>,
) -> Vec<SenderBaseNonce> {
    let mut nonces = HashMap::<usize, Vec<u64>>::new();
    for block in blocks {
        for tx in &block.body {
            if let Some(sender_index) = address_index.get(&tx.sender) {
                nonces
                    .entry(*sender_index)
                    .or_default()
                    .push(tx.nonce().low_u64());
            }
        }
    }
    let mut out = Vec::new();
    for (sender_index, values) in nonces {
        let base_nonce = values.iter().copied().min().unwrap_or(0);
        let saving: isize = values
            .iter()
            .map(|nonce| {
                cfxpack::codec::uleb128_len(*nonce) as isize
                    - cfxpack::codec::uleb128_len(nonce.saturating_sub(base_nonce)) as isize
            })
            .sum();
        if saving >= 16 {
            out.push(SenderBaseNonce {
                sender_index,
                base_nonce,
            });
        }
    }
    out.sort_by_key(|entry| entry.sender_index);
    out
}

fn build_pos_lookup(
    blocks: &[RawBlock],
    pos_blocks: &HashMap<H256, CommittedBlock>,
    min_pos_height: u64,
    pos_reference_enable_height: u64,
) -> Result<Vec<PosLookupEntry>> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for block in blocks {
        if let Some(pos_ref) = effective_pos_reference(&block.header, pos_reference_enable_height) {
            if seen.insert(*pos_ref) {
                let committed = pos_blocks
                    .get(pos_ref)
                    .ok_or_else(|| anyhow!("missing cached committed block"))?;
                let height_offset = committed
                    .view
                    .checked_sub(min_pos_height)
                    .ok_or_else(|| anyhow!("PoS view below min_pos_height"))?;
                out.push(PosLookupEntry {
                    hash: *pos_ref,
                    height_offset: u16::try_from(height_offset)
                        .context("PoS height offset exceeds u16")?,
                });
            }
        }
    }
    Ok(out)
}

/// Insertion-order-stable frequency counter: ranks values by descending count,
/// breaking ties by first-seen order so the resulting table is deterministic.
#[derive(Default)]
struct Frequency<T> {
    counts: HashMap<T, (usize, usize)>,
    next_order: usize,
}

impl<T> Frequency<T>
where
    T: Eq + std::hash::Hash + Copy,
{
    fn add(&mut self, value: T) {
        let order = self.next_order;
        self.counts
            .entry(value)
            .and_modify(|entry| entry.0 += 1)
            .or_insert_with(|| {
                self.next_order += 1;
                (1, order)
            });
    }

    fn into_sorted(self) -> Vec<T> {
        self.into_sorted_with_counts()
            .into_iter()
            .map(|(value, _)| value)
            .collect()
    }

    fn into_sorted_with_counts(self) -> Vec<(T, usize)> {
        let mut values = self
            .counts
            .into_iter()
            .map(|(value, (count, order))| (value, count, order))
            .collect::<Vec<_>>();
        values.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.2.cmp(&b.2)));
        values
            .into_iter()
            .map(|(value, count, _)| (value, count))
            .collect()
    }
}
