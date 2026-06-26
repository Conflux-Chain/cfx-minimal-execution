use super::ExtractConfig;
use anyhow::{anyhow, Context, Result};
use cfx_types::H256;
use cfxpack::packet::{PosRewardAccount, PosRewardEntry, UnlockEntry};
use diem_types::committed_block::CommittedBlock;
use primitives::{Block, BlockHeader, SignedTransaction};
use rlp::Rlp;
use rocksdb::{
    rocksdb_options::ColumnFamilyDescriptor, BlockBasedOptions, Cache, ColumnFamilyOptions,
    DBIterator, DBOptions, LRUCacheOptions, ReadOptions, SeekKey, DB,
};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};

pub(super) const EPOCH_EXECUTED_BLOCK_SET_SUFFIX: u8 = 0x06;
const COL_BLOCKS: &str = "col1";
const COL_EPOCH_NUMBERS: &str = "col3";
// reward_by_pos_epoch (DESIGN §8.1, CF ID 7): PoS epoch (8-byte BE) -> RLP PosRewardInfo.
const COL_REWARD_BY_POS_EPOCH: &str = "col7";
const EPOCH_SKIPPED_BLOCK_SET_SUFFIX: u8 = 0x07;
const BLOCK_BODY_SUFFIX: u8 = 0x02;
const EPOCH_EXECUTION_CONTEXT_SUFFIX: u8 = 0x04;
const BLOCK_REWARD_RESULT_SUFFIX: u8 = 0x08;

#[derive(Debug, Clone, Default)]
pub(super) struct EpochExecutionContext {
    pub(super) start_block_number: u64,
}

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct BlockRewardResult {
    pub(super) base_reward: cfx_types::U256,
    pub(super) total_reward: cfx_types::U256,
}

#[derive(Debug)]
pub(super) struct EpochBlocks {
    pub(super) number: u64,
    pub(super) executed: Vec<H256>,
    pub(super) skipped: Vec<H256>,
}

#[derive(Debug, Clone)]
struct DbPaths {
    blockchain_db: PathBuf,
    pos_ledger_db: PathBuf,
}

impl DbPaths {
    fn from_data_dir(data_dir: &Path) -> Self {
        Self {
            blockchain_db: data_dir.join("blockchain_data/blockchain_db"),
            pos_ledger_db: data_dir.join("pos_db/db/pos-ledger-db"),
        }
    }
}

pub(super) fn open_databases(config: &ExtractConfig) -> Result<(PowDb, PosDb)> {
    let paths = DbPaths::from_data_dir(&config.data_dir);
    let pow = PowDb::open(&paths.blockchain_db)?;
    let pos = PosDb::open(&paths.pos_ledger_db)?;
    Ok((pow, pos))
}

pub(super) struct PowDb {
    db: DB,
    read_opts: ReadOptions,
    _block_caches: Vec<Cache>,
}

unsafe impl Send for PowDb {}
unsafe impl Sync for PowDb {}

impl PowDb {
    fn open(path: &Path) -> Result<Self> {
        let cf_names = [
            "col0", "col1", "col2", "col3", "col4", "col5", "col6", "col7",
        ];
        let (cf, block_caches) = read_only_cf_descriptors(cf_names);
        let mut opts = DBOptions::default();
        opts.create_if_missing(false);
        let db = DB::open_cf_for_read_only(opts, path.to_str().unwrap(), cf, false)
            .map_err(|e| anyhow!("open PoW DB {}: {e}", path.display()))?;
        Ok(Self {
            db,
            read_opts: no_cache_read_options(),
            _block_caches: block_caches,
        })
    }

    fn get(&self, cf: &str, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let handle = self
            .db
            .cf_handle(cf)
            .ok_or_else(|| anyhow!("missing CF {cf}"))?;
        self.db
            .get_cf_opt(handle, key, &self.read_opts)
            .map(|value| value.map(|value| value.to_vec()))
            .map_err(|e| anyhow!("read CF {cf}: {e}"))
    }

    /// Scan the `reward_by_pos_epoch` CF (col7) and return every PoS interest
    /// distribution whose `execution_epoch_hash` is in `pivots`, keyed by that
    /// pivot hash. Value is RLP `PosRewardInfo` (production `block_data_types.rs`,
    /// `#[derive(RlpEncodable)]`): `[ [ [addr, pos_id, reward], ... ], exec_hash ]`.
    /// Full-CF scan — fine for bounded ranges; revisit for whole-chain runs.
    pub(super) fn read_pos_rewards(
        &self,
        pivots: &HashSet<H256>,
    ) -> Result<HashMap<H256, PosRewardEntry>> {
        let mut out = HashMap::new();
        if pivots.is_empty() {
            return Ok(out);
        }
        let handle = self
            .db
            .cf_handle(COL_REWARD_BY_POS_EPOCH)
            .ok_or_else(|| anyhow!("missing CF {COL_REWARD_BY_POS_EPOCH}"))?;
        let mut iter = DBIterator::new_cf(&self.db, handle, no_cache_read_options());
        iter.seek(SeekKey::Start)
            .map_err(|e| anyhow!("seek col7: {e}"))?;
        while iter.valid().map_err(|e| anyhow!("iter col7: {e}"))? {
            let value = iter.value().to_vec();
            let rlp = Rlp::new(&value);
            let execution_epoch_hash: H256 =
                rlp.val_at(1).context("decode PosRewardInfo exec hash")?;
            if pivots.contains(&execution_epoch_hash) {
                let rewards_rlp = rlp.at(0).context("decode PosRewardInfo account_rewards")?;
                let mut account_rewards = Vec::with_capacity(rewards_rlp.item_count()?);
                for j in 0..rewards_rlp.item_count()? {
                    let a = rewards_rlp.at(j)?;
                    account_rewards.push(PosRewardAccount {
                        address: a.val_at(0).context("pos reward address")?,
                        pos_identifier: a.val_at(1).context("pos reward identifier")?,
                        reward: a.val_at(2).context("pos reward amount")?,
                    });
                }
                out.insert(
                    execution_epoch_hash,
                    PosRewardEntry {
                        account_rewards,
                        execution_epoch_hash,
                    },
                );
            }
            iter.next().map_err(|e| anyhow!("iter col7 next: {e}"))?;
        }
        Ok(out)
    }
}

pub(super) struct PosDb {
    db: DB,
    read_opts: ReadOptions,
    _block_caches: Vec<Cache>,
}

unsafe impl Send for PosDb {}
unsafe impl Sync for PosDb {}

impl PosDb {
    fn open(path: &Path) -> Result<Self> {
        let cf_names = [
            "default",
            "epoch_by_version",
            "event_accumulator",
            "event_by_key",
            "event_by_version",
            "event",
            "jellyfish_merkle_node",
            "ledger_counters",
            "stale_node_index",
            "transaction",
            "transaction_accumulator",
            "transaction_by_account",
            "transaction_info",
            "ledger_info_by_block",
            "pos_state",
            "reward_event",
            "committed_block",
            "committed_block_by_view",
            "ledger_info_by_voted_block",
            "block_by_epoch_and_round",
        ];
        let (cf, block_caches) = read_only_cf_descriptors(cf_names);
        let mut opts = DBOptions::default();
        opts.create_if_missing(false);
        let db = DB::open_cf_for_read_only(opts, path.to_str().unwrap(), cf, false)
            .map_err(|e| anyhow!("open PoS DB {}: {e}", path.display()))?;
        Ok(Self {
            db,
            read_opts: no_cache_read_options(),
            _block_caches: block_caches,
        })
    }

    pub(super) fn get_committed_block(&self, hash: &H256) -> Result<CommittedBlock> {
        let handle = self
            .db
            .cf_handle("committed_block")
            .ok_or_else(|| anyhow!("missing committed_block CF"))?;
        let value = self
            .db
            .get_cf_opt(handle, hash.as_bytes(), &self.read_opts)
            .map_err(|e| anyhow!("read committed_block: {e}"))?
            .ok_or_else(|| anyhow!("missing committed_block for {hash:?}"))?;
        bcs::from_bytes(&value).context("decode committed_block")
    }

    /// Scan the `event` CF for UnlockEvent entries in the version range
    /// `[start_version, end_version)` — matching production's
    /// `get_events(parent_pos_ref, current_pos_ref)` semantics. The event CF
    /// key is `version(BE u64) || index(BE u64)` and the value is BCS-encoded
    /// `ContractEvent`.
    pub(super) fn read_unlock_events(
        &self,
        start_version: u64,
        end_version: u64,
    ) -> Result<Vec<UnlockEntry>> {
        if start_version >= end_version {
            return Ok(Vec::new());
        }
        let handle = self
            .db
            .cf_handle("event")
            .ok_or_else(|| anyhow!("missing event CF"))?;
        let mut iter = DBIterator::new_cf(&self.db, handle, no_cache_read_options());
        let seek_key = start_version.to_be_bytes();
        iter.seek(SeekKey::Key(&seek_key))
            .map_err(|e| anyhow!("seek event CF: {e}"))?;
        let mut out = Vec::new();
        while iter.valid().map_err(|e| anyhow!("iter event CF: {e}"))? {
            let key = iter.key();
            if key.len() < 8 {
                break;
            }
            let version = u64::from_be_bytes(key[..8].try_into().unwrap());
            if version >= end_version {
                break;
            }
            let value = iter.value();
            if let Some(entry) = try_decode_unlock_event(value) {
                out.push(entry);
            }
            iter.next()
                .map_err(|e| anyhow!("iter event CF next: {e}"))?;
        }
        Ok(out)
    }
}

/// The BCS-encoded UnlockEvent event key (40 bytes): salt=5 (LE u64) ||
/// address 0x1DC (32 bytes, left-padded). Used to filter ContractEvent entries.
const UNLOCK_EVENT_KEY: [u8; 40] = {
    let mut key = [0u8; 40];
    key[0] = 5; // salt=5 LE
    key[39] = 0xDC; // address 0x1DC low byte
    key[38] = 0x01; // address 0x1DC high byte
    key
};

/// Try to decode a BCS ContractEvent value as an UnlockEvent. Returns None if
/// the event key doesn't match UnlockEvent.
fn try_decode_unlock_event(value: &[u8]) -> Option<UnlockEntry> {
    // BCS layout: [0x00 (V0 variant)] [0x28 (EventKey serde_bytes length=40)]
    //             [40 bytes EventKey data] [uleb128 len] [event_data bytes]
    if value.len() < 43 || value[0] != 0 || value[1] != 40 {
        return None;
    }
    if value[2..42] != UNLOCK_EVENT_KEY {
        return None;
    }
    let mut pos = 42;
    let data_len = bcs_read_uleb128(value, &mut pos)?;
    if pos + data_len > value.len() {
        return None;
    }
    let event_data = &value[pos..pos + data_len];
    // UnlockEvent BCS: [32 bytes AccountAddress (node_id)] [u64 LE unlocked]
    if event_data.len() < 40 {
        return None;
    }
    let identifier = H256::from_slice(&event_data[..32]);
    let unlocked = u64::from_le_bytes(event_data[32..40].try_into().ok()?);
    Some(UnlockEntry {
        identifier,
        unlocked,
    })
}

fn bcs_read_uleb128(data: &[u8], pos: &mut usize) -> Option<usize> {
    let mut result: usize = 0;
    let mut shift = 0;
    loop {
        if *pos >= data.len() {
            return None;
        }
        let byte = data[*pos];
        *pos += 1;
        result |= ((byte & 0x7F) as usize) << shift;
        if byte & 0x80 == 0 {
            return Some(result);
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
}

fn read_only_cf_descriptors<const N: usize>(
    names: [&'static str; N],
) -> (Vec<ColumnFamilyDescriptor<'static>>, Vec<Cache>) {
    let mut descriptors = Vec::with_capacity(N);
    let mut caches = Vec::with_capacity(N);
    for name in names {
        let mut cache_opts = LRUCacheOptions::new();
        cache_opts.set_capacity(4 * 1024 * 1024);
        let cache = Cache::new_lru_cache(cache_opts);

        let mut block_opts = BlockBasedOptions::new();
        block_opts.set_block_cache(&cache);
        block_opts.set_cache_index_and_filter_blocks(true);
        block_opts.set_cache_index_and_filter_blocks_with_high_priority(true);

        let mut cf_opts = ColumnFamilyOptions::default();
        cf_opts.set_block_based_table_factory(&block_opts);
        descriptors.push(ColumnFamilyDescriptor::new(name, cf_opts));
        caches.push(cache);
    }
    (descriptors, caches)
}

fn no_cache_read_options() -> ReadOptions {
    let mut opts = ReadOptions::new();
    opts.fill_cache(false);
    opts.set_verify_checksums(false);
    opts
}

pub(super) fn load_epoch(pow: &PowDb, epoch: u64) -> Result<EpochBlocks> {
    let executed = read_epoch_hashes(pow, epoch, EPOCH_EXECUTED_BLOCK_SET_SUFFIX)?
        .ok_or_else(|| anyhow!("missing epoch {epoch} executed block set"))?;
    let skipped =
        read_epoch_hashes(pow, epoch, EPOCH_SKIPPED_BLOCK_SET_SUFFIX)?.unwrap_or_default();
    Ok(EpochBlocks {
        number: epoch,
        executed,
        skipped,
    })
}

pub(super) fn read_epoch_hashes(pow: &PowDb, epoch: u64, suffix: u8) -> Result<Option<Vec<H256>>> {
    let mut key = epoch.to_le_bytes().to_vec();
    key.push(suffix);
    pow.get(COL_EPOCH_NUMBERS, &key)?
        .map(|bytes| {
            Rlp::new(&bytes)
                .as_list::<H256>()
                .context("decode epoch block hashes")
        })
        .transpose()
}

fn block_key(hash: &H256, suffix: Option<u8>) -> Vec<u8> {
    let mut key = hash.as_bytes().to_vec();
    if let Some(suffix) = suffix {
        key.push(suffix);
    }
    key
}

pub(super) fn read_header(pow: &PowDb, hash: &H256) -> Result<BlockHeader> {
    let bytes = pow
        .get(COL_BLOCKS, &block_key(hash, None))?
        .ok_or_else(|| anyhow!("missing block header {hash:?}"))?;
    // Headers are stored *with* the pow_hash field (index 14), so the network
    // `Decodable` (which has no pow_hash and reads pos_reference at 14) shifts
    // every field after nonce by one — silently misreading pow_hash as
    // pos_reference and the real pos_reference as base_price. Pre-PoS blocks
    // hid this because the misread pos_reference is unused there. Use the
    // pow-hash-aware decoder that matches the stored layout.
    BlockHeader::decode_with_pow_hash(&bytes)
        .with_context(|| format!("decode block header {hash:?}"))
}

pub(super) fn read_body(pow: &PowDb, hash: &H256) -> Result<Vec<Arc<SignedTransaction>>> {
    let Some(bytes) = pow.get(COL_BLOCKS, &block_key(hash, Some(BLOCK_BODY_SUFFIX)))? else {
        return Ok(Vec::new());
    };
    Block::decode_body_with_tx_public(&Rlp::new(&bytes))
        .with_context(|| format!("decode block body {hash:?}"))
}

pub(super) fn read_epoch_context(pow: &PowDb, pivot_hash: &H256) -> Result<EpochExecutionContext> {
    let bytes = pow
        .get(
            COL_BLOCKS,
            &block_key(pivot_hash, Some(EPOCH_EXECUTION_CONTEXT_SUFFIX)),
        )?
        .ok_or_else(|| anyhow!("missing epoch execution context {pivot_hash:?}"))?;
    let rlp = Rlp::new(&bytes);
    Ok(EpochExecutionContext {
        start_block_number: rlp
            .val_at(0)
            .context("decode epoch execution context start_block_number")?,
    })
}

pub(super) fn read_reward(pow: &PowDb, hash: &H256) -> Result<Option<BlockRewardResult>> {
    let Some(bytes) = pow.get(
        COL_BLOCKS,
        &block_key(hash, Some(BLOCK_REWARD_RESULT_SUFFIX)),
    )?
    else {
        return Ok(None);
    };
    let tuple = Rlp::new(&bytes);
    let reward = tuple.at(1).context("decode block reward tuple value")?;
    // BlockRewardResult is RLP-encoded as [total_reward, base_reward, tx_fee].
    Ok(Some(BlockRewardResult {
        total_reward: reward
            .val_at(0)
            .context("decode block reward total_reward")?,
        base_reward: reward
            .val_at(1)
            .context("decode block reward base_reward")?,
    }))
}
