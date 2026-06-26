use anyhow::{anyhow, ensure, Context, Result};
use cfx_types::{Space, H256};
use cfxpack::packet::{
    encode_packet, Block, Packet, FLAG_ADAPTIVE, FLAG_ESPACE, FLAG_PIVOT, FLAG_SKIPPED_EXECUTION,
    FLAG_ZERO_TOTAL_REWARD,
};
use diem_types::committed_block::CommittedBlock;
use primitives::{block_header::CIP112_TRANSITION_HEIGHT, BlockHeader, SignedTransaction};
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
mod tables;

pub use flagpatch::{add_total_reward_flag, FlagPatchSummary};

use db::{
    load_epoch, open_databases, read_body, read_epoch_context, read_epoch_hashes, read_header,
    read_reward, BlockRewardResult, EpochBlocks, PosDb, PowDb, EPOCH_EXECUTED_BLOCK_SET_SUFFIX,
};
use pack::run_packed;
pub use pack::{PackSummary, DEFAULT_PACK_TARGET_BYTES};
use shards::run_shards;
use tables::build_tables;

#[derive(Debug, Clone)]
pub struct ExtractConfig {
    pub data_dir: PathBuf,
    pub start_epoch: u64,
    pub epoch_count: u64,
    pub chain: ChainParams,
}

/// Conflux chain parameters that affect field extraction. Loaded from a TOML
/// config file (see [`ChainParams::from_toml_file`]) rather than a long list of
/// CLI flags, so a full-chain run carries an auditable, version-controlled set
/// of mainnet activation heights.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ChainParams {
    /// espace flag: a pivot block at `height % ratio == 0` carries eSpace txs.
    pub evm_transaction_block_ratio: u64,
    /// Below this height a block's `pos_reference` is treated as None, so
    /// `finalized_epoch` is 0. Mainnet: 37400000.
    pub pos_reference_enable_height: u64,
    /// Height at which the block-header `custom` field encoding is fixed
    /// (CIP-112). Headers at/after this height decode wrong without it.
    /// Mainnet: 79050000.
    pub cip112_transition_height: u64,
}

impl Default for ChainParams {
    fn default() -> Self {
        Self {
            evm_transaction_block_ratio: 5,
            pos_reference_enable_height: u64::MAX,
            cip112_transition_height: u64::MAX,
        }
    }
}

impl ChainParams {
    /// Parse a TOML config file holding the chain parameters.
    pub fn from_toml_file(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read chain config {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parse chain config {}", path.display()))
    }
}

impl Default for ExtractConfig {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("data/blockchain_data"),
            start_epoch: 1,
            epoch_count: 2,
            chain: ChainParams::default(),
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
    Databases::open(config)?.to_file(config, output)
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
    let _ = CIP112_TRANSITION_HEIGHT.set(config.chain.cip112_transition_height);

    let output_dir = output_dir.as_ref().to_path_buf();
    std::fs::create_dir_all(&output_dir)
        .with_context(|| format!("create {}", output_dir.display()))?;
    let dbs = Databases::open(config)?;

    let mut reports = run_shards(config, &output_dir, shard_epochs, jobs, &dbs)?;
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
    let _ = CIP112_TRANSITION_HEIGHT.set(config.chain.cip112_transition_height);

    let output_dir = output_dir.as_ref().to_path_buf();
    std::fs::create_dir_all(&output_dir)
        .with_context(|| format!("create {}", output_dir.display()))?;
    let dbs = Databases::open(config)?;

    run_packed(
        config,
        &output_dir,
        shard_epochs,
        jobs,
        target_bytes,
        prefix,
        &dbs,
    )
}

pub fn extract_packet(config: &ExtractConfig) -> Result<Vec<u8>> {
    let (packet, _) = Databases::open(config)?.encoded(config)?;
    Ok(packet)
}

pub fn extract_raw_data(config: &ExtractConfig) -> Result<Packet> {
    let (raw, _) = Databases::open(config)?.raw(config)?;
    Ok(raw)
}

/// The node's PoW + PoS databases, opened once and reused across an extraction.
/// Holding them in a handle keeps the entry points free of the "open vs. inject
/// the databases" distinction that used to leak into helper names.
pub(super) struct Databases {
    pow: PowDb,
    pos: PosDb,
}

impl Databases {
    pub(super) fn open(config: &ExtractConfig) -> Result<Self> {
        let (pow, pos) = open_databases(config)?;
        Ok(Self { pow, pos })
    }

    /// Extract one 2000-epoch group and write its packet to `output`.
    pub(super) fn to_file(
        &self,
        config: &ExtractConfig,
        output: impl AsRef<Path>,
    ) -> Result<ExtractReport> {
        let (packet, mut report) = self.encoded(config)?;
        let output = output.as_ref().to_path_buf();
        let started = Instant::now();
        std::fs::write(&output, &packet)
            .with_context(|| format!("write packet {}", output.display()))?;
        report.timing.write_ms = started.elapsed().as_millis();
        report.output = output;
        Ok(report)
    }

    /// Extract one 2000-epoch group, returning the encoded packet bytes and a
    /// report, without touching the disk.
    pub(super) fn encoded(&self, config: &ExtractConfig) -> Result<(Vec<u8>, ExtractReport)> {
        let (raw, mut timing) = self.raw(config)?;
        let started = Instant::now();
        let packet = encode_packet(&raw)?;
        timing.encode_ms = started.elapsed().as_millis();

        let started = Instant::now();
        let verify = cfxpack::verify::verify_packet(&packet)?;
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

    /// Read one 2000-epoch group from the node DB into the in-memory [`Packet`],
    /// alongside a per-phase timing breakdown.
    fn raw(&self, config: &ExtractConfig) -> Result<(Packet, ExtractTiming)> {
        extract_raw(config, &self.pow, &self.pos)
    }
}

fn extract_raw(
    config: &ExtractConfig,
    pow: &PowDb,
    pos: &PosDb,
) -> Result<(Packet, ExtractTiming)> {
    ensure!(config.epoch_count > 0, "epoch_count must be positive");
    let _ = CIP112_TRANSITION_HEIGHT.set(config.chain.cip112_transition_height);
    let mut timing = ExtractTiming::default();

    let epochs = time(&mut timing.load_epochs_ms, || load_epochs(pow, config))?;
    let boundary = read_prev_boundary(pow, config, &epochs)?;

    let CollectedBlocks {
        block_inputs,
        min_timestamp,
        min_height,
        min_pos_height,
        pos_blocks,
    } = time(&mut timing.read_blocks_ms, || {
        collect_blocks(config, pow, pos, &epochs)
    })?;

    let tables = time(&mut timing.build_tables_ms, || {
        build_tables(
            &block_inputs,
            &pos_blocks,
            min_pos_height,
            config.chain.pos_reference_enable_height,
        )
    })?;

    let mut blocks = time(&mut timing.build_blocks_ms, || {
        build_block_inputs(config, pow, &pos_blocks, block_inputs)
    })?;

    // Attach PoS interest distributions (DESIGN §8.8): production stores each in
    // the reward_by_pos_epoch CF tagged with the pivot hash of the PoW epoch it
    // was distributed in, so a block whose hash matches gets the entry. Applied
    // by the executor at that epoch's settlement.
    let pivots: HashSet<H256> = blocks
        .iter()
        .filter(|b| b.flags & FLAG_PIVOT != 0)
        .map(|b| b.hash)
        .collect();
    let pos_rewards = pow.read_pos_rewards(&pivots)?;
    if !pos_rewards.is_empty() {
        for block in blocks.iter_mut() {
            if let Some(entry) = pos_rewards.get(&block.hash) {
                block.pos_rewards.push(entry.clone());
            }
        }
    }

    // Attach PoS unlock events (DESIGN §8.8): production processes unlock events
    // when a pivot's pos_reference differs from its parent's. We scan the PoS
    // event CF between consecutive pos_reference versions and attach unlock
    // entries to the corresponding epoch's last (pivot) block.
    let unlock_map = collect_unlock_events(config, pow, pos, &pos_blocks, &epochs)?;
    if !unlock_map.is_empty() {
        for block in blocks.iter_mut() {
            if block.flags & FLAG_PIVOT != 0 {
                if let Some(unlocks) = unlock_map.get(&block.epoch) {
                    block.unlock_events = unlocks.clone();
                }
            }
        }
    }

    Ok((
        Packet {
            prev_last_hash: boundary.prev_last_hash,
            prev_last_deferred_state_root: boundary.prev_last_deferred_state_root,
            first_block_number: boundary.first_block_number,
            min_timestamp,
            min_height,
            min_pos_height,
            addresses: tables.addresses,
            pos_entries: tables.pos_entries,
            difficulties: tables.difficulties,
            sender_base_nonces: tables.sender_base_nonces,
            gas_prices: tables.gas_prices,
            blocks,
        },
        timing,
    ))
}

/// Run `f`, recording its wall-clock duration (ms) into `slot`. Lets the
/// orchestration above stay a flat list of phases instead of interleaving
/// `Instant::now()` bookkeeping with the work.
fn time<T>(slot: &mut u128, f: impl FnOnce() -> Result<T>) -> Result<T> {
    let started = Instant::now();
    let out = f()?;
    *slot = started.elapsed().as_millis();
    Ok(out)
}

/// Phase 1: read the executed/skipped block sets for every epoch in range.
fn load_epochs(pow: &PowDb, config: &ExtractConfig) -> Result<Vec<EpochBlocks>> {
    let mut epochs = Vec::new();
    for epoch in config.start_epoch..config.start_epoch + config.epoch_count {
        epochs.push(load_epoch(pow, epoch)?);
    }
    Ok(epochs)
}

/// The cross-group boundary: the prior epoch's pivot anchors this group's first
/// state root, and its executed block set fixes the starting block number.
struct PrevBoundary {
    prev_last_hash: H256,
    prev_last_deferred_state_root: H256,
    first_block_number: u64,
}

fn read_prev_boundary(
    pow: &PowDb,
    config: &ExtractConfig,
    epochs: &[EpochBlocks],
) -> Result<PrevBoundary> {
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

    Ok(PrevBoundary {
        prev_last_hash,
        prev_last_deferred_state_root: *prev_last_header.deferred_state_root(),
        first_block_number,
    })
}

/// Output of the read-blocks phase: the per-block raw inputs plus the range
/// minimums and PoS-block cache the later phases need.
struct CollectedBlocks {
    block_inputs: Vec<RawBlock>,
    min_timestamp: u64,
    min_height: u64,
    min_pos_height: u64,
    pos_blocks: HashMap<H256, CommittedBlock>,
}

/// Phase 2: read every block's header/body/reward, derive its flags, and cache
/// the PoS committed blocks referenced along the way.
fn collect_blocks(
    config: &ExtractConfig,
    pow: &PowDb,
    pos: &PosDb,
    epochs: &[EpochBlocks],
) -> Result<CollectedBlocks> {
    let mut block_inputs = Vec::new();
    let mut min_timestamp = u64::MAX;
    let mut min_height = u64::MAX;
    let mut min_pos_height = u64::MAX;
    let mut pos_blocks = HashMap::<H256, CommittedBlock>::new();

    for epoch in epochs {
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
                effective_pos_reference(&header, config.chain.pos_reference_enable_height)
            {
                let committed = pos.get_committed_block(pos_ref)?;
                min_pos_height = min_pos_height.min(committed.view);
                pos_blocks.insert(*pos_ref, committed);
            }
            let flags = block_flags(
                config,
                &header,
                &reward,
                hash == pivot_hash,
                &skipped,
                &hash,
            );
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
    if min_pos_height == u64::MAX {
        min_pos_height = 0;
    }

    Ok(CollectedBlocks {
        block_inputs,
        min_timestamp,
        min_height,
        min_pos_height,
        pos_blocks,
    })
}

fn block_flags(
    config: &ExtractConfig,
    header: &BlockHeader,
    reward: &BlockRewardResult,
    pivot: bool,
    skipped: &HashSet<H256>,
    hash: &H256,
) -> u8 {
    let mut flags = 0u8;
    if header.adaptive() {
        flags |= FLAG_ADAPTIVE;
    }
    if pivot {
        flags |= FLAG_PIVOT;
    }
    if pivot && header.height() % config.chain.evm_transaction_block_ratio == 0 {
        flags |= FLAG_ESPACE;
    }
    if skipped.contains(hash) {
        flags |= FLAG_SKIPPED_EXECUTION;
    }
    // Corner-case marker: the block's full settled reward is zero.
    if reward.total_reward.is_zero() {
        flags |= FLAG_ZERO_TOTAL_REWARD;
    }
    flags
}

/// Phase 4: lower each [`RawBlock`] into the packet's [`Block`], resolving
/// the PoS-derived finalized-epoch offset per block.
fn build_block_inputs(
    config: &ExtractConfig,
    pow: &PowDb,
    pos_blocks: &HashMap<H256, CommittedBlock>,
    block_inputs: Vec<RawBlock>,
) -> Result<Vec<Block>> {
    let mut blocks = Vec::with_capacity(block_inputs.len());
    for (index, raw) in block_inputs.into_iter().enumerate() {
        let base_prices = raw.header.base_price();
        let finalized_epoch = finalized_epoch_offset(
            pow,
            pos_blocks,
            &raw,
            config.chain.pos_reference_enable_height,
        )?;
        let pos_view =
            effective_pos_reference(&raw.header, config.chain.pos_reference_enable_height)
                .and_then(|pr| pos_blocks.get(pr))
                .map(|c| c.view);
        blocks.push(Block {
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
            pos_rewards: Vec::new(),
            unlock_events: Vec::new(),
            pos_view,
        });
    }
    Ok(blocks)
}

/// Per-block raw inputs read straight from the node DB, before lowering into the
/// packet's [`Block`]. Shared by the table builders in [`tables`].
#[derive(Debug)]
pub(super) struct RawBlock {
    epoch: u64,
    hash: H256,
    // Read by the table builders in `tables`; the rest stay module-private.
    pub(super) header: BlockHeader,
    pub(super) body: Vec<Arc<SignedTransaction>>,
    reward: BlockRewardResult,
    flags: u8,
}

fn finalized_epoch_offset(
    pow: &PowDb,
    pos_blocks: &HashMap<H256, CommittedBlock>,
    block: &RawBlock,
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
    // conflux sets `env.finalized_epoch` directly to the pivot-decision block's
    // height (see consensus_executor epoch_execution.rs:
    // `finalized_epoch: pivot_decision_epoch` = `header.height()`), with NO
    // `pos_pivot_decision_defer_epoch_count` applied at this stage — the defer
    // was already baked into which block the PoS layer chose as the pivot
    // decision. We encode the offset the executor inverts (`pivot.epoch -
    // offset`), so `offset = block.epoch - pd_height`. Subtracting defer here
    // (the old behaviour) made finalized_epoch wrong by 50/20 and also would
    // have needed CIP-113 height-dependent switching; neither is correct.
    let pd_height = pivot_header.height();
    Ok(block.epoch.saturating_sub(pd_height))
}

pub(super) fn effective_pos_reference(
    header: &BlockHeader,
    pos_reference_enable_height: u64,
) -> Option<&H256> {
    if header.height() < pos_reference_enable_height {
        None
    } else {
        header.pos_reference().as_ref()
    }
}

/// Collect PoS unlock events by scanning pos_reference transitions across
/// consecutive pivot blocks in the packet. Returns a map: epoch_number →
/// Vec<UnlockEntry>.
fn collect_unlock_events(
    config: &ExtractConfig,
    pow: &PowDb,
    pos: &PosDb,
    pos_blocks: &HashMap<H256, CommittedBlock>,
    epochs: &[EpochBlocks],
) -> Result<HashMap<u64, Vec<cfxpack::packet::UnlockEntry>>> {
    let pos_enable = config.chain.pos_reference_enable_height;

    // Get the previous epoch's pivot pos_reference as the starting point.
    let prev_epoch = config.start_epoch.saturating_sub(1);
    let prev_hashes = read_epoch_hashes(pow, prev_epoch, EPOCH_EXECUTED_BLOCK_SET_SUFFIX)?
        .ok_or_else(|| anyhow!("missing previous epoch {} for unlock events", prev_epoch))?;
    let prev_pivot = prev_hashes
        .last()
        .ok_or_else(|| anyhow!("previous epoch {} has no pivot", prev_epoch))?;
    let prev_header = read_header(pow, prev_pivot)?;
    let mut last_pos_ref: Option<H256> = effective_pos_reference(&prev_header, pos_enable).copied();
    // The previous epoch's pos_reference may not be in pos_blocks (which only
    // covers the current packet), so look it up from the PoS DB on demand.
    let mut last_version: Option<u64> = last_pos_ref
        .as_ref()
        .map(|h| pos.get_committed_block(h).map(|cb| cb.version))
        .transpose()?;

    let mut result = HashMap::new();

    for epoch in epochs {
        let pivot_hash = epoch
            .executed
            .last()
            .ok_or_else(|| anyhow!("epoch {} has no pivot", epoch.number))?;
        let header = read_header(pow, pivot_hash)?;
        let current_pos_ref = effective_pos_reference(&header, pos_enable).copied();

        if let (Some(prev_ver), Some(curr)) = (last_version, current_pos_ref.as_ref()) {
            if last_pos_ref.as_ref() != Some(curr) {
                if let Some(curr_cb) = pos_blocks.get(curr) {
                    let unlocks = pos.read_unlock_events(prev_ver, curr_cb.version)?;
                    if !unlocks.is_empty() {
                        result.insert(epoch.number, unlocks);
                    }
                }
            }
        }

        if let Some(curr) = current_pos_ref.as_ref() {
            last_version = pos_blocks.get(curr).map(|cb| cb.version);
        }
        last_pos_ref = current_pos_ref;
    }

    Ok(result)
}
