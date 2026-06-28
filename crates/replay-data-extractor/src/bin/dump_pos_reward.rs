use anyhow::{anyhow, Context, Result};
use rlp::Rlp;
use rocksdb::{
    rocksdb_options::ColumnFamilyDescriptor, BlockBasedOptions, Cache, ColumnFamilyOptions,
    DBIterator, DBOptions, LRUCacheOptions, ReadOptions, SeekKey, DB,
};
use std::env;

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("get") if args.len() >= 4 => get(&args[2], args[3].parse().context("parse pos_epoch")?),
        Some("scan_all") if args.len() >= 3 => scan_all(&args[2]),
        _ => {
            eprintln!("Usage:");
            eprintln!("  dump_pos_reward get <blockchain_db_path> <pos_epoch>");
            eprintln!("  dump_pos_reward scan_all <blockchain_db_path>");
            eprintln!("    Iterates col7 and prints: pos_epoch execution_epoch_hash");
            std::process::exit(1);
        }
    }
}

fn open_db(db_path: &str) -> Result<DB> {
    let cf_names = [
        "col0", "col1", "col2", "col3", "col4", "col5", "col6", "col7",
    ];
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
    DB::open_cf_for_read_only(opts, db_path, cfs, false)
        .map_err(|e| anyhow!("open DB {}: {e}", db_path))
}

fn scan_all(db_path: &str) -> Result<()> {
    let db = open_db(db_path)?;
    let handle = db
        .cf_handle("col7")
        .ok_or_else(|| anyhow!("missing col7"))?;

    let mut read_opts = ReadOptions::new();
    read_opts.fill_cache(false);
    read_opts.set_verify_checksums(false);
    let mut iter = DBIterator::new_cf(&db, handle, read_opts);
    iter.seek(SeekKey::Start)
        .map_err(|e| anyhow!("seek col7: {e}"))?;
    let mut count = 0u64;
    while iter.valid().map_err(|e| anyhow!("iter col7: {e}"))? {
        let key = iter.key();
        let value = iter.value();
        if key.len() == 8 {
            let pos_epoch = u64::from_be_bytes(key.try_into().unwrap());
            let rlp = Rlp::new(value);
            if let Ok(exec_hash_bytes) = rlp.val_at::<Vec<u8>>(1) {
                println!("{} 0x{}", pos_epoch, hex::encode(&exec_hash_bytes));
                count += 1;
            }
        }
        iter.next()
            .map_err(|e| anyhow!("iter next col7: {e}"))?;
    }
    eprintln!("total: {} entries", count);
    Ok(())
}

fn get(db_path: &str, pos_epoch: u64) -> Result<()> {
    let db = open_db(db_path)?;
    let handle = db
        .cf_handle("col7")
        .ok_or_else(|| anyhow!("missing col7"))?;

    let key = pos_epoch.to_be_bytes();
    let read_opts = ReadOptions::default();
    let value = db
        .get_cf_opt(handle, &key, &read_opts)
        .map_err(|e| anyhow!("get col7: {e}"))?
        .ok_or_else(|| anyhow!("no entry for PoS epoch {pos_epoch}"))?;

    let rlp = Rlp::new(&value);
    let exec_hash_bytes: Vec<u8> = rlp.val_at(1).context("decode exec hash")?;
    let exec_hash = format!("0x{}", hex::encode(&exec_hash_bytes));

    let rewards_rlp = rlp.at(0).context("decode account_rewards")?;
    let count = rewards_rlp.item_count()?;

    println!("{{");
    println!("  \"pos_epoch\": {},", pos_epoch);
    println!("  \"execution_epoch_hash\": \"{}\",", exec_hash);
    println!("  \"account_rewards\": [");
    for j in 0..count {
        let a = rewards_rlp.at(j)?;
        let addr_bytes: Vec<u8> = a.val_at(0).context("address")?;
        let id_bytes: Vec<u8> = a.val_at(1).context("identifier")?;
        let reward_bytes: Vec<u8> = a.val_at(2).context("reward")?;
        let reward = cfx_types::U256::from_big_endian(&reward_bytes);
        let comma = if j + 1 < count { "," } else { "" };
        println!(
            "    {{\"address\": \"0x{}\", \"pos_identifier\": \"0x{}\", \"reward\": \"{}\"}}{comma}",
            hex::encode(&addr_bytes),
            hex::encode(&id_bytes),
            reward
        );
    }
    println!("  ]");
    println!("}}");

    Ok(())
}
