// Read epoch's on-chain executed/skipped block set and each block's body
// straight from blockchain_db, printing REAL block hashes and REAL (signed) tx
// hashes. Used to cross-check cfxpack faithfulness against raw node data.
//
// Usage: dump_epoch <blockchain_db_path> <epoch>
use anyhow::{anyhow, Context, Result};
use cfx_types::{Space, H256};
use primitives::{Block, SignedTransaction};
use rlp::Rlp;
use rocksdb::{
    rocksdb_options::ColumnFamilyDescriptor, BlockBasedOptions, Cache, ColumnFamilyOptions,
    DBOptions, LRUCacheOptions, ReadOptions, DB,
};
use std::env;
use std::sync::Arc;

const COL_BLOCKS: &str = "col1";
const COL_EPOCH_NUMBERS: &str = "col3";

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: dump_epoch <blockchain_db_path> <epoch> [end_epoch] [espace_only]");
        std::process::exit(1);
    }
    let db = open_db(&args[1])?;
    let start: u64 = args[2].parse().context("parse epoch")?;
    let end: u64 = args.get(3).map(|s| s.parse()).transpose()?.unwrap_or(start);
    let espace_only = args.get(4).map(|s| s == "espace").unwrap_or(false);

    for epoch in start..=end {
        let executed = read_epoch_hashes(&db, epoch, 0x06)?.unwrap_or_default();
        let skipped = read_epoch_hashes(&db, epoch, 0x07)?.unwrap_or_default();
        println!("=== epoch {epoch}: executed={} skipped={} ===", executed.len(), skipped.len());
        if let Some(pivot) = executed.last() {
            match read_deferred_roots(&db, pivot) {
                Ok((sr, rr, lr)) => println!(
                    "    pivot={pivot:?} deferred_state_root={sr:?} deferred_receipts_root={rr:?} deferred_logs_bloom_hash={lr:?}"
                ),
                Err(e) => println!("    pivot_hdr read error: {e}"),
            }
        }
        for (i, h) in executed.iter().enumerate() {
            let body = read_body(&db, h)?;
            let is_pivot = i + 1 == executed.len();
            let shown: Vec<_> = body
                .iter()
                .enumerate()
                .filter(|(_, tx)| !espace_only || tx.space() == Space::Ethereum)
                .collect();
            if espace_only && shown.is_empty() {
                continue;
            }
            println!("[{i}] block={h:?} pivot={is_pivot} height_mod5={} txs={}", 0, body.len());
            for (j, tx) in shown {
                println!(
                    "    tx[{j}] hash={:?} space={:?} sender={:?} nonce={} action={:?} value={}",
                    tx.hash(), tx.space(), tx.sender(), tx.nonce(), tx.action(), tx.value()
                );
            }
        }
        for (i, h) in skipped.iter().enumerate() {
            let body = read_body(&db, h)?;
            let shown: Vec<_> = body
                .iter()
                .enumerate()
                .filter(|(_, tx)| !espace_only || tx.space() == Space::Ethereum)
                .collect();
            if espace_only && shown.is_empty() {
                continue;
            }
            println!("[skip {i}] block={h:?} txs={}", body.len());
            for (j, tx) in shown {
                println!(
                    "    tx[{j}] hash={:?} space={:?} sender={:?} nonce={} action={:?} value={}",
                    tx.hash(), tx.space(), tx.sender(), tx.nonce(), tx.action(), tx.value()
                );
            }
        }
    }
    Ok(())
}

fn read_epoch_hashes(db: &DB, epoch: u64, suffix: u8) -> Result<Option<Vec<H256>>> {
    let mut key = epoch.to_le_bytes().to_vec();
    key.push(suffix);
    let handle = db.cf_handle(COL_EPOCH_NUMBERS).ok_or_else(|| anyhow!("no col3"))?;
    let opts = read_opts();
    db.get_cf_opt(handle, &key, &opts)
        .map_err(|e| anyhow!("read col3: {e}"))?
        .map(|bytes| Rlp::new(&bytes).as_list::<H256>().context("decode hashes"))
        .transpose()
}

// Raw-RLP read of the three deferred roots only. The stored header RLP layout
// is: [parent_hash, height, timestamp, author, transactions_root,
//      deferred_state_root(5), deferred_receipts_root(6),
//      deferred_logs_bloom_hash(7), ...]. These are all BEFORE `nonce`, so the
// stored-header pow_hash-at-14 shift does not affect them, and we avoid the
// full BlockHeader::decode_with_pow_hash path (which panics on some headers).
fn read_deferred_roots(db: &DB, hash: &H256) -> Result<(H256, H256, H256)> {
    let handle = db.cf_handle(COL_BLOCKS).ok_or_else(|| anyhow!("no col1"))?;
    let opts = read_opts();
    let bytes = db
        .get_cf_opt(handle, hash.as_bytes(), &opts)
        .map_err(|e| anyhow!("read col1 header: {e}"))?
        .ok_or_else(|| anyhow!("missing header {hash:?}"))?;
    let rlp = Rlp::new(&bytes);
    Ok((
        rlp.val_at(5).context("deferred_state_root")?,
        rlp.val_at(6).context("deferred_receipts_root")?,
        rlp.val_at(7).context("deferred_logs_bloom_hash")?,
    ))
}

fn read_body(db: &DB, hash: &H256) -> Result<Vec<Arc<SignedTransaction>>> {
    let mut key = hash.as_bytes().to_vec();
    key.push(0x02);
    let handle = db.cf_handle(COL_BLOCKS).ok_or_else(|| anyhow!("no col1"))?;
    let opts = read_opts();
    let Some(bytes) = db
        .get_cf_opt(handle, &key, &opts)
        .map_err(|e| anyhow!("read col1: {e}"))?
    else {
        return Ok(Vec::new());
    };
    Block::decode_body_with_tx_public(&Rlp::new(&bytes)).context("decode body")
}

fn read_opts() -> ReadOptions {
    let mut o = ReadOptions::new();
    o.fill_cache(false);
    o.set_verify_checksums(false);
    o
}

fn open_db(db_path: &str) -> Result<DB> {
    let cf_names = ["col0", "col1", "col2", "col3", "col4", "col5", "col6", "col7"];
    let mut cfs = Vec::new();
    for name in cf_names {
        let mut cf_opts = ColumnFamilyOptions::default();
        let mut cache_opts = LRUCacheOptions::new();
        cache_opts.set_capacity(8 << 20);
        let cache = Cache::new_lru_cache(cache_opts);
        let mut block_opts = BlockBasedOptions::default();
        block_opts.set_block_cache(&cache);
        cf_opts.set_block_based_table_factory(&block_opts);
        cfs.push(ColumnFamilyDescriptor::new(name, cf_opts));
    }
    let mut opts = DBOptions::default();
    opts.create_if_missing(false);
    DB::open_cf_for_read_only(opts, db_path, cfs, false).map_err(|e| anyhow!("open DB {}: {e}", db_path))
}
