use crate::{
    packet::{
        BlockInput, PacketInput, PosLookupEntry, SenderBaseNonce, FLAG_ADAPTIVE, FLAG_ESPACE,
        FLAG_PIVOT, FLAG_SKIPPED_EXECUTION, FLAG_ZERO_TOTAL_REWARD,
    },
    raw::{encode_raw_data, RawExecutionData},
};
use anyhow::{anyhow, ensure, Context, Result};
use cfx_types::{Address, Space, H256, U256};
use diem_types::committed_block::CommittedBlock;
use primitives::{block_header::CIP112_TRANSITION_HEIGHT, Action, BlockHeader, SignedTransaction};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

mod db;
mod flagpatch;
mod pack;
mod shards;

pub use flagpatch::{add_total_reward_flag, FlagPatchSummary};

use db::{
    load_epoch, open_databases, read_body, read_epoch_context, read_epoch_hashes, read_header,
    read_reward, BlockRewardResult, PosDb, PowDb, EPOCH_EXECUTED_BLOCK_SET_SUFFIX,
};
pub use pack::{PackSummary, DEFAULT_PACK_TARGET_BYTES};
use pack::run_packed;
use shards::run_shards;

#[derive(Debug, Clone)]
pub struct ExtractConfig {
    pub data_dir: PathBuf,
    pub start_epoch: u64,
    pub epoch_count: u64,
    pub evm_transaction_block_ratio: u64,
    pub pos_pivot_decision_defer_epoch_count: u64,
    pub pos_reference_enable_height: u64,
    pub cip112_transition_height: u64,
}

impl Default for ExtractConfig {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("data/blockchain_data"),
            start_epoch: 1,
            epoch_count: 2,
            evm_transaction_block_ratio: 5,
            pos_pivot_decision_defer_epoch_count: 50,
            pos_reference_enable_height: u64::MAX,
            cip112_transition_height: u64::MAX,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ExtractReport {
    pub start_epoch: u64,
    pub epoch_count: u64,
    pub block_count: usize,
    pub transaction_count: usize,
    pub packet_bytes: usize,
    pub output: PathBuf,
    pub timing: ExtractTiming,
}

#[derive(Debug, Clone, Default)]
pub struct ExtractTiming {
    pub load_epochs_ms: u128,
    pub read_blocks_ms: u128,
    pub build_tables_ms: u128,
    pub build_blocks_ms: u128,
    pub encode_ms: u128,
    pub verify_ms: u128,
    pub write_ms: u128,
}

pub fn extract_to_file(config: &ExtractConfig, output: impl AsRef<Path>) -> Result<ExtractReport> {
    let (pow, pos) = open_databases(config)?;
    extract_to_file_with_dbs(config, output, &pow, &pos)
}

pub fn extract_shards_to_dir(
    config: &ExtractConfig,
    output_dir: impl AsRef<Path>,
    shard_epochs: u64,
    jobs: usize,
) -> Result<Vec<ExtractReport>> {
    ensure!(config.epoch_count > 0, "epoch_count must be positive");
    ensure!(shard_epochs > 0, "shard_epochs must be positive");
    ensure!(jobs > 0, "jobs must be positive");
    let _ = CIP112_TRANSITION_HEIGHT.set(config.cip112_transition_height);

    let output_dir = output_dir.as_ref().to_path_buf();
    std::fs::create_dir_all(&output_dir)
        .with_context(|| format!("create {}", output_dir.display()))?;
    let (pow, pos) = open_databases(config)?;

    let mut reports = run_shards(config, &output_dir, shard_epochs, jobs, &pow, &pos)?;
    reports.sort_by_key(|report| report.start_epoch);
    Ok(reports)
}

/// Extract `config.epoch_count` epochs as 2000-epoch groups (unchanged spec),
/// packing consecutive groups into ~`target_bytes` container files named
/// `<prefix>_<start_epoch>_<end_epoch>.cfxpack`, each carrying a directory that
/// allows direct lookup of any single 2000-epoch group.
pub fn extract_packed_to_dir(
    config: &ExtractConfig,
    output_dir: impl AsRef<Path>,
    shard_epochs: u64,
    jobs: usize,
    target_bytes: u64,
    prefix: &str,
) -> Result<PackSummary> {
    ensure!(config.epoch_count > 0, "epoch_count must be positive");
    ensure!(shard_epochs > 0, "shard_epochs must be positive");
    ensure!(jobs > 0, "jobs must be positive");
    ensure!(target_bytes > 0, "target_bytes must be positive");
    let _ = CIP112_TRANSITION_HEIGHT.set(config.cip112_transition_height);

    let output_dir = output_dir.as_ref().to_path_buf();
    std::fs::create_dir_all(&output_dir)
        .with_context(|| format!("create {}", output_dir.display()))?;
    let (pow, pos) = open_databases(config)?;

    run_packed(
        config,
        &output_dir,
        shard_epochs,
        jobs,
        target_bytes,
        prefix,
        &pow,
        &pos,
    )
}

fn extract_to_file_with_dbs(
    config: &ExtractConfig,
    output: impl AsRef<Path>,
    pow: &PowDb,
    pos: &PosDb,
) -> Result<ExtractReport> {
    let (packet, mut report) = extract_packet_with_report(config, pow, pos)?;

    let output = output.as_ref().to_path_buf();
    let started = Instant::now();
    std::fs::write(&output, &packet)
        .with_context(|| format!("write packet {}", output.display()))?;
    report.timing.write_ms = started.elapsed().as_millis();
    report.output = output;
    Ok(report)
}

/// Extract a single 2000-epoch group, returning the exact packet bytes and a
/// report. The packet byte layout is the unchanged 2000-epoch group spec; this
/// path only differs from `extract_to_file_with_dbs` in not touching the disk.
fn extract_packet_with_report(
    config: &ExtractConfig,
    pow: &PowDb,
    pos: &PosDb,
) -> Result<(Vec<u8>, ExtractReport)> {
    let (raw, mut timing) = extract_raw_data_with_timing(config, pow, pos)?;
    let started = Instant::now();
    let packet = encode_raw_data(&raw)?;
    timing.encode_ms = started.elapsed().as_millis();

    let started = Instant::now();
    let verify = crate::verify::verify_packet(&packet)?;
    timing.verify_ms = started.elapsed().as_millis();

    let report = ExtractReport {
        start_epoch: config.start_epoch,
        epoch_count: config.epoch_count,
        block_count: verify.block_count as usize,
        transaction_count: verify.transaction_items as usize,
        packet_bytes: packet.len(),
        output: PathBuf::new(),
        timing,
    };
    Ok((packet, report))
}

pub fn extract_packet(config: &ExtractConfig) -> Result<Vec<u8>> {
    let (pow, pos) = open_databases(config)?;
    extract_packet_with_dbs(config, &pow, &pos)
}

fn extract_packet_with_dbs(config: &ExtractConfig, pow: &PowDb, pos: &PosDb) -> Result<Vec<u8>> {
    let raw = extract_raw_data_with_dbs(config, pow, pos)?;
    encode_raw_data(&raw)
}

pub fn extract_raw_data(config: &ExtractConfig) -> Result<RawExecutionData> {
    let (pow, pos) = open_databases(config)?;
    extract_raw_data_with_dbs(config, &pow, &pos)
}

fn extract_raw_data_with_dbs(
    config: &ExtractConfig,
    pow: &PowDb,
    pos: &PosDb,
) -> Result<RawExecutionData> {
    extract_raw_data_with_timing(config, pow, pos).map(|(raw, _)| raw)
}

fn extract_raw_data_with_timing(
    config: &ExtractConfig,
    pow: &PowDb,
    pos: &PosDb,
) -> Result<(RawExecutionData, ExtractTiming)> {
    ensure!(config.epoch_count > 0, "epoch_count must be positive");
    let _ = CIP112_TRANSITION_HEIGHT.set(config.cip112_transition_height);
    let mut timing = ExtractTiming::default();

    let started = Instant::now();
    let mut epochs = Vec::new();
    for epoch in config.start_epoch..config.start_epoch + config.epoch_count {
        epochs.push(load_epoch(pow, epoch)?);
    }
    timing.load_epochs_ms = started.elapsed().as_millis();

    let prev_epoch = config.start_epoch.saturating_sub(1);
    let prev_hashes = read_epoch_hashes(pow, prev_epoch, EPOCH_EXECUTED_BLOCK_SET_SUFFIX)?
        .ok_or_else(|| anyhow!("missing previous epoch {} executed block set", prev_epoch))?;
    let prev_last_hash = *prev_hashes
        .last()
        .ok_or_else(|| anyhow!("previous epoch {} has no pivot block", prev_epoch))?;
    let prev_last_header = read_header(pow, &prev_last_hash)?;

    let first_pivot = epochs
        .first()
        .and_then(|e| e.executed.last())
        .ok_or_else(|| anyhow!("first epoch has no pivot block"))?;
    let first_block_number = read_epoch_context(pow, first_pivot)
        .with_context(|| format!("read first epoch context for {first_pivot:?}"))?
        .start_block_number;

    let mut block_inputs = Vec::new();
    let mut min_timestamp = u64::MAX;
    let mut min_height = u64::MAX;
    let mut min_pos_height = u64::MAX;
    let mut pos_blocks = HashMap::<H256, CommittedBlock>::new();

    let started = Instant::now();
    for epoch in &epochs {
        let skipped = epoch.skipped.iter().copied().collect::<HashSet<_>>();
        let pivot_hash = epoch
            .executed
            .last()
            .copied()
            .ok_or_else(|| anyhow!("epoch {} has no pivot block", epoch.number))?;
        let mut ordered = epoch.executed.clone();
        ordered.pop();
        ordered.extend(epoch.skipped.iter().copied());
        ordered.push(pivot_hash);
        for hash in ordered {
            let header = read_header(pow, &hash)?;
            let body = read_body(pow, &hash)?;
            let reward = read_reward(pow, &hash)?.unwrap_or_default();
            min_timestamp = min_timestamp.min(header.timestamp());
            min_height = min_height.min(header.height());
            if let Some(pos_ref) =
                effective_pos_reference(&header, config.pos_reference_enable_height)
            {
                let committed = pos.get_committed_block(pos_ref)?;
                min_pos_height = min_pos_height.min(committed.view);
                pos_blocks.insert(*pos_ref, committed);
            }
            let pivot = hash == pivot_hash;
            let mut flags = 0u8;
            if header.adaptive() {
                flags |= FLAG_ADAPTIVE;
            }
            if pivot {
                flags |= FLAG_PIVOT;
            }
            if pivot && header.height() % config.evm_transaction_block_ratio == 0 {
                flags |= FLAG_ESPACE;
            }
            if skipped.contains(&hash) {
                flags |= FLAG_SKIPPED_EXECUTION;
            }
            // Corner-case marker: the block's full settled reward is zero.
            if reward.total_reward.is_zero() {
                flags |= FLAG_ZERO_TOTAL_REWARD;
            }
            block_inputs.push(RawBlock {
                epoch: epoch.number,
                hash,
                header,
                body,
                reward,
                flags,
            });
        }
    }
    timing.read_blocks_ms = started.elapsed().as_millis();
    if min_pos_height == u64::MAX {
        min_pos_height = 0;
    }

    let started = Instant::now();
    let addresses = build_address_table(&block_inputs);
    let address_index = addresses
        .iter()
        .copied()
        .enumerate()
        .map(|(i, address)| (address, i))
        .collect::<HashMap<_, _>>();
    let difficulties = build_difficulty_table(&block_inputs);
    let gas_prices = build_gas_price_table(&block_inputs);
    let sender_base_nonces = build_sender_base_nonce_table(&block_inputs, &address_index);
    let pos_entries = build_pos_lookup(
        &block_inputs,
        &pos_blocks,
        min_pos_height,
        config.pos_reference_enable_height,
    )?;
    timing.build_tables_ms = started.elapsed().as_millis();

    let started = Instant::now();
    let mut blocks = Vec::with_capacity(block_inputs.len());
    for (index, raw) in block_inputs.into_iter().enumerate() {
        let base_prices = raw.header.base_price();
        let finalized_epoch = finalized_epoch_offset(
            pow,
            &pos_blocks,
            &raw,
            config.pos_pivot_decision_defer_epoch_count,
            config.pos_reference_enable_height,
        )?;
        blocks.push(BlockInput {
            epoch: raw.epoch,
            index,
            hash: raw.hash,
            deferred_state_root: *raw.header.deferred_state_root(),
            deferred_receipts_root: *raw.header.deferred_receipts_root(),
            deferred_logs_bloom_hash: *raw.header.deferred_logs_bloom_hash(),
            flags: raw.flags,
            author: *raw.header.author(),
            timestamp: raw.header.timestamp(),
            difficulty: *raw.header.difficulty(),
            gas_limit: *raw.header.gas_limit(),
            base_price_core: base_prices
                .map(|p| *p.in_space(Space::Native))
                .unwrap_or_default(),
            base_price_espace: base_prices
                .map(|p| *p.in_space(Space::Ethereum))
                .unwrap_or_default(),
            height: raw.header.height(),
            blame: raw.header.blame() as u64,
            finalized_epoch,
            base_reward: raw.reward.base_reward,
            transactions: raw.body.into_iter().map(|tx| (*tx).clone()).collect(),
            transaction_refs: Vec::new(),
        });
    }
    timing.build_blocks_ms = started.elapsed().as_millis();

    Ok((
        PacketInput {
            prev_last_hash,
            prev_last_deferred_state_root: *prev_last_header.deferred_state_root(),
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
        },
        timing,
    ))
}

#[derive(Debug)]
struct RawBlock {
    epoch: u64,
    hash: H256,
    header: BlockHeader,
    body: Vec<Arc<SignedTransaction>>,
    reward: BlockRewardResult,
    flags: u8,
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
                crate::codec::uleb128_len(*nonce) as isize
                    - crate::codec::uleb128_len(nonce.saturating_sub(base_nonce)) as isize
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

fn finalized_epoch_offset(
    pow: &PowDb,
    pos_blocks: &HashMap<H256, CommittedBlock>,
    block: &RawBlock,
    defer: u64,
    pos_reference_enable_height: u64,
) -> Result<u64> {
    let Some(pos_ref) = effective_pos_reference(&block.header, pos_reference_enable_height) else {
        return Ok(0);
    };
    let committed = pos_blocks
        .get(pos_ref)
        .ok_or_else(|| anyhow!("missing committed block for finalized_epoch"))?;
    let pivot_hash = committed.pivot_decision.block_hash;
    let pivot_header = read_header(pow, &pivot_hash)?;
    let finalized_height = pivot_header.height().saturating_sub(defer);
    Ok(block.epoch.saturating_sub(finalized_height))
}

fn effective_pos_reference(
    header: &BlockHeader,
    pos_reference_enable_height: u64,
) -> Option<&H256> {
    if header.height() < pos_reference_enable_height {
        None
    } else {
        header.pos_reference().as_ref()
    }
}

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
