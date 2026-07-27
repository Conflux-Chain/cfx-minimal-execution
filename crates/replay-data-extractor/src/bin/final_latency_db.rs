// Finalization latency read STRAIGHT from the node databases (no cfxpack).
//
// Mirrors `cfxpack`'s `epoch_final_latency`, but derives each pivot epoch's
// finalized epoch the same way the extractor does: header `pos_reference` ->
// PoS `committed_block` (pos-ledger-db) -> `pivot_decision.height`. So it needs
// BOTH the PoW blockchain_db and the pos-ledger-db.
//
//   fin_abs(E) = pivot_decision.height  (deepest finalized epoch as of pivot E)
//   offset(E)  = E - fin_abs(E)
//   lag_time(E)/lag_epochs(E): time/epoch gap until a later pivot first reports
//                              fin_abs >= E (running-max staircase).
// Only pivot blocks count (non-pivot referee finalized_epoch is not finality).
//
// Data dir layout (same as the extractor's DbPaths::from_data_dir):
//   <data_dir>/blockchain_data/blockchain_db
//   <data_dir>/pos_db/db/pos-ledger-db
//
// Usage:
//   final_latency_db <data_dir> <days>                      # trailing-N-days window
//   final_latency_db <data_dir> monthly [n_months] [pos_h]  # last N complete UTC+8 months + current
use anyhow::{anyhow, Context, Result};
use cfx_types::H256;
use diem_types::committed_block::CommittedBlock;
use primitives::block_header::CIP112_TRANSITION_HEIGHT;
use primitives::BlockHeader;
use rlp::Rlp;
use rocksdb::{
    rocksdb_options::ColumnFamilyDescriptor, ColumnFamilyOptions, DBOptions, ReadOptions, DB,
};
use std::collections::HashMap;
use std::env;
use std::path::Path;
use std::sync::Arc;
use std::thread;

const COL_BLOCKS: &str = "col1";
const COL_EPOCH_NUMBERS: &str = "col3";
const EXECUTED_SUFFIX: u8 = 0x06;
// Conflux mainnet chain params (config/mainnet.toml). pos_reference_enable_height
// gates whether a header's pos_reference counts; CIP112 fixes header decoding at
// or above its height and MUST be set before any BlockHeader::decode_with_pow_hash.
const DEFAULT_POS_H: u64 = 37_400_000;
const CIP112_TRANSITION: u64 = 79_050_000;
const DEFAULT_THREADS: usize = 24;

struct DbWrap(DB);
// Read-only concurrent gets are safe; mirror the extractor's PowDb/PosDb.
unsafe impl Send for DbWrap {}
unsafe impl Sync for DbWrap {}

struct Dbs {
    pow: DbWrap,
    pos: DbWrap,
}

fn main() -> Result<()> {
    // Required before decoding any stored header (see block_header.rs).
    let _ = CIP112_TRANSITION_HEIGHT.set(CIP112_TRANSITION);
    let args: Vec<String> = env::args().collect();
    let data_dir = args
        .get(1)
        .ok_or_else(|| anyhow!("usage: final_latency_db <data_dir> <days>|monthly [n_months] [pos_h]"))?;
    let pow_path = Path::new(data_dir).join("blockchain_data/blockchain_db");
    let pos_path = Path::new(data_dir).join("pos_db/db/pos-ledger-db");
    anyhow::ensure!(pow_path.is_dir(), "no blockchain_db at {}", pow_path.display());
    anyhow::ensure!(pos_path.is_dir(), "no pos-ledger-db at {}", pos_path.display());
    eprintln!("pow={} pos={}", pow_path.display(), pos_path.display());
    let dbs = Arc::new(Dbs {
        pow: DbWrap(open_pow(pow_path.to_str().unwrap())?),
        pos: DbWrap(open_pos(pos_path.to_str().unwrap())?),
    });

    let tip = find_tip(&dbs.pow.0)?;
    let tip_ts = epoch_pivot_timestamp(&dbs.pow.0, tip)?.ok_or_else(|| anyhow!("no tip ts"))?;

    match args.get(2).map(|s| s.as_str()) {
        Some("monthly") => {
            let n_months: u64 = args.get(3).map(|s| s.parse()).transpose()?.unwrap_or(12);
            let pos_h: u64 = args.get(4).map(|s| s.parse()).transpose()?.unwrap_or(DEFAULT_POS_H);
            let windows = monthly_windows(tip_ts, n_months);
            let oldest_lo = windows.first().map(|w| w.1).unwrap_or(0);
            let e_start = find_start_epoch(&dbs.pow.0, oldest_lo, tip)? + 1;
            eprintln!(
                "tip={tip} tip_ts={tip_ts} pos_h={pos_h} epochs[{e_start}..{tip}] months[{}..{}] (trailing '*' = current incomplete)",
                windows.first().map(|w| w.0.as_str()).unwrap_or("-"),
                windows.last().map(|w| w.0.as_str()).unwrap_or("-"),
            );
            let pivots = collect_pivots(Arc::clone(&dbs), e_start, tip, pos_h, DEFAULT_THREADS)?;
            let (steps, max_fin) = build_staircase(&pivots);
            println!("month     n_epochs  pend  |  time_s: mean p50 p90 p99 max  |  epochs: mean p50 p90 p99 max");
            for (label, ts_lo, ts_hi) in &windows {
                emit_row(label, *ts_lo, *ts_hi, &pivots, &steps, max_fin);
            }
        }
        Some(days_str) => {
            let days: u64 = days_str.parse().context("days")?;
            let pos_h: u64 = args.get(3).map(|s| s.parse()).transpose()?.unwrap_or(DEFAULT_POS_H);
            let lo_ts = tip_ts.saturating_sub(days * 86400);
            let e_start = find_start_epoch(&dbs.pow.0, lo_ts, tip)? + 1;
            eprintln!("tip={tip} tip_ts={tip_ts} pos_h={pos_h} epochs[{e_start}..{tip}] last {days}d");
            let pivots = collect_pivots(Arc::clone(&dbs), e_start, tip, pos_h, DEFAULT_THREADS)?;
            let (steps, max_fin) = build_staircase(&pivots);
            println!("window    n_epochs  pend  |  time_s: mean p50 p90 p99 max  |  epochs: mean p50 p90 p99 max");
            emit_row(&format!("last{days}d"), lo_ts, tip_ts + 1, &pivots, &steps, max_fin);
        }
        None => return Err(anyhow!("usage: final_latency_db <data_dir> <days>|monthly [n_months] [pos_h]")),
    }
    Ok(())
}

/// Per-pivot (epoch, timestamp, fin_abs), pivot-only, sorted ascending by ts.
/// fin_abs = pos_reference -> committed_block.pivot_decision.height.
fn collect_pivots(
    dbs: Arc<Dbs>,
    start: u64,
    end: u64,
    pos_h: u64,
    threads: usize,
) -> Result<Vec<(u64, u64, u64)>> {
    let total = end - start + 1;
    let chunk = total.div_ceil(threads as u64);
    let mut handles = Vec::new();
    for t in 0..threads as u64 {
        let s = start + t * chunk;
        if s > end {
            break;
        }
        let e = (s + chunk - 1).min(end);
        let dbs = Arc::clone(&dbs);
        handles.push(thread::spawn(move || worker(&dbs, s, e, pos_h)));
    }
    let mut out: Vec<(u64, u64, u64)> = Vec::new();
    for h in handles {
        out.extend(h.join().map_err(|_| anyhow!("worker panicked"))??);
    }
    out.sort_unstable_by_key(|&(_e, ts, _f)| ts);
    Ok(out)
}

fn worker(dbs: &Dbs, start: u64, end: u64, pos_h: u64) -> Result<Vec<(u64, u64, u64)>> {
    let mut out = Vec::new();
    let mut cache: HashMap<H256, u64> = HashMap::new(); // pos_ref -> pivot_decision.height
    for epoch in start..=end {
        let Some(hashes) = read_epoch_hashes(&dbs.pow.0, epoch, EXECUTED_SUFFIX)? else {
            continue;
        };
        let Some(pivot) = hashes.last() else { continue };
        let Some(bytes) = read_header_bytes(&dbs.pow.0, pivot)? else { continue };
        let header = match BlockHeader::decode_with_pow_hash(&bytes) {
            Ok(h) => h,
            Err(_) => continue,
        };
        let ts = header.timestamp();
        let height = header.height();
        // effective_pos_reference: None before PoS activation.
        if height < pos_h {
            continue;
        }
        let Some(pos_ref) = header.pos_reference().as_ref() else { continue };
        let pd_height = match cache.get(pos_ref) {
            Some(&h) => h,
            None => {
                let committed = get_committed_block(&dbs.pos.0, pos_ref)?;
                let h = committed.pivot_decision.height;
                cache.insert(*pos_ref, h);
                h
            }
        };
        // fin_abs = pd_height; use epoch (== pivot height) as E.
        out.push((epoch, ts, pd_height));
    }
    Ok(out)
}

/// Running-max staircase of fin_abs over pivots (already ts-sorted): each strict
/// increase is a step (fin_reached, ts, epoch). Returns (steps, max_finalized).
fn build_staircase(pivots: &[(u64, u64, u64)]) -> (Vec<(u64, u64, u64)>, u64) {
    let mut steps = Vec::new();
    let mut run_max = 0u64;
    for &(epoch, ts, fin_abs) in pivots {
        if fin_abs >= epoch {
            continue; // no/degenerate pos_reference
        }
        if fin_abs > run_max {
            run_max = fin_abs;
            steps.push((run_max, ts, epoch));
        }
    }
    (steps, run_max)
}

fn emit_row(
    label: &str,
    ts_lo: u64,
    ts_hi: u64,
    pivots: &[(u64, u64, u64)],
    steps: &[(u64, u64, u64)],
    max_fin: u64,
) {
    let mut lag_time: Vec<f64> = Vec::new();
    let mut lag_epochs: Vec<f64> = Vec::new();
    let mut pending = 0u64;
    let lo = pivots.partition_point(|&(_e, ts, _f)| ts < ts_lo);
    for &(epoch, pts, _f) in &pivots[lo..] {
        if pts >= ts_hi {
            break;
        }
        if epoch > max_fin {
            pending += 1;
            continue;
        }
        let i = steps.partition_point(|&(f, _, _)| f < epoch);
        lag_time.push(steps[i].1.saturating_sub(pts) as f64);
        lag_epochs.push(steps[i].2.saturating_sub(epoch) as f64);
    }
    if lag_time.is_empty() {
        println!("{label}  (no in-window finalized epochs)");
        return;
    }
    lag_time.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap());
    lag_epochs.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap());
    let n = lag_time.len();
    let mt = |q| pctl(&lag_time, q);
    let me = |q| pctl(&lag_epochs, q);
    let mean_t = lag_time.iter().sum::<f64>() / n as f64;
    let mean_e = lag_epochs.iter().sum::<f64>() / n as f64;
    println!(
        "{label}  {n:>8}  {pending:>4}  |  {mean_t:6.1} {:4.0} {:4.0} {:4.0} {:5.0}  |  {mean_e:6.1} {:4.0} {:4.0} {:4.0} {:5.0}",
        mt(0.50), mt(0.90), mt(0.99), lag_time[n - 1],
        me(0.50), me(0.90), me(0.99), lag_epochs[n - 1],
    );
}

fn pctl(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    sorted[(q * (sorted.len() - 1) as f64).round() as usize]
}

// ---- UTC+8 calendar (no chrono dep), identical to epoch_final_latency ----

fn monthly_windows(max_ts: u64, n: u64) -> Vec<(String, u64, u64)> {
    let cur_days = (max_ts as i64 + 8 * 3600).div_euclid(86400);
    let (y, m, _d) = civil_from_days(cur_days);
    let mut out = Vec::new();
    for k in 1..=n as i64 {
        let (yy, mm) = add_months(y, m, -k);
        let (ny, nm) = add_months(yy, mm, 1);
        out.push((format!("{yy:04}-{mm:02}"), month_start_ts(yy, mm), month_start_ts(ny, nm)));
    }
    out.reverse();
    let (ny, nm) = add_months(y, m, 1);
    out.push((format!("{y:04}-{m:02}*"), month_start_ts(y, m), month_start_ts(ny, nm)));
    out
}

fn month_start_ts(y: i64, m: i64) -> u64 {
    (days_from_civil(y, m, 1) * 86400 - 8 * 3600) as u64
}

fn add_months(y: i64, m: i64, delta: i64) -> (i64, i64) {
    let idx = y * 12 + (m - 1) + delta;
    (idx.div_euclid(12), idx.rem_euclid(12) + 1)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ---- blockchain_db / pos-ledger-db access ----

fn find_tip(db: &DB) -> Result<u64> {
    let has = |e: u64| -> Result<bool> { Ok(read_epoch_hashes(db, e, EXECUTED_SUFFIX)?.is_some()) };
    let mut lo: u64 = 1;
    let mut step: u64 = 1_000_000;
    let mut hi;
    loop {
        let probe = lo + step;
        if has(probe)? {
            lo = probe;
            step = step.saturating_mul(2);
        } else {
            hi = probe;
            break;
        }
    }
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if has(mid)? {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Ok(lo)
}

fn find_start_epoch(db: &DB, target_ts: u64, tip: u64) -> Result<u64> {
    let mut lo: u64 = 1;
    let mut hi: u64 = tip;
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        match epoch_pivot_timestamp(db, mid)? {
            Some(ts) if ts <= target_ts => lo = mid,
            _ => hi = mid,
        }
    }
    Ok(lo)
}

fn epoch_pivot_timestamp(db: &DB, epoch: u64) -> Result<Option<u64>> {
    let Some(hashes) = read_epoch_hashes(db, epoch, EXECUTED_SUFFIX)? else {
        return Ok(None);
    };
    let Some(pivot) = hashes.last() else { return Ok(None) };
    let bytes = read_header_bytes(db, pivot)?.ok_or_else(|| anyhow!("missing pivot header"))?;
    Ok(Some(Rlp::new(&bytes).val_at::<u64>(2).context("timestamp")?))
}

fn read_epoch_hashes(db: &DB, epoch: u64, suffix: u8) -> Result<Option<Vec<H256>>> {
    let mut key = epoch.to_le_bytes().to_vec();
    key.push(suffix);
    let handle = db.cf_handle(COL_EPOCH_NUMBERS).ok_or_else(|| anyhow!("no col3"))?;
    db.get_cf_opt(handle, &key, &read_opts())
        .map_err(|e| anyhow!("read col3: {e}"))?
        .map(|bytes| Rlp::new(&bytes).as_list::<H256>().context("decode hashes"))
        .transpose()
}

fn read_header_bytes(db: &DB, hash: &H256) -> Result<Option<Vec<u8>>> {
    let handle = db.cf_handle(COL_BLOCKS).ok_or_else(|| anyhow!("no col1"))?;
    db.get_cf_opt(handle, hash.as_bytes(), &read_opts())
        .map(|o| o.map(|v| v.to_vec()))
        .map_err(|e| anyhow!("read col1: {e}"))
}

fn get_committed_block(db: &DB, hash: &H256) -> Result<CommittedBlock> {
    let handle = db.cf_handle("committed_block").ok_or_else(|| anyhow!("no committed_block CF"))?;
    let value = db
        .get_cf_opt(handle, hash.as_bytes(), &read_opts())
        .map_err(|e| anyhow!("read committed_block: {e}"))?
        .ok_or_else(|| anyhow!("missing committed_block {hash:?}"))?;
    bcs::from_bytes(&value).context("decode committed_block")
}

fn read_opts() -> ReadOptions {
    let mut o = ReadOptions::new();
    o.fill_cache(false);
    o.set_verify_checksums(false);
    o
}

fn open_pow(path: &str) -> Result<DB> {
    open_cf(path, &["col0", "col1", "col2", "col3", "col4", "col5", "col6", "col7"])
}

fn open_pos(path: &str) -> Result<DB> {
    open_cf(
        path,
        &[
            "default", "epoch_by_version", "event_accumulator", "event_by_key",
            "event_by_version", "event", "jellyfish_merkle_node", "ledger_counters",
            "stale_node_index", "transaction", "transaction_accumulator",
            "transaction_by_account", "transaction_info", "ledger_info_by_block",
            "pos_state", "reward_event", "committed_block", "committed_block_by_view",
            "ledger_info_by_voted_block", "block_by_epoch_and_round",
        ],
    )
}

fn open_cf(path: &str, cf_names: &[&str]) -> Result<DB> {
    let cfs: Vec<_> = cf_names
        .iter()
        .map(|name| ColumnFamilyDescriptor::new(*name, ColumnFamilyOptions::default()))
        .collect();
    let mut opts = DBOptions::default();
    opts.create_if_missing(false);
    // Cap the table cache so read-only open of a full-chain DB (thousands of
    // SSTs) does not blow past the process file-descriptor limit ("Too many
    // open files"). Two DBs are opened at once, so keep this modest.
    opts.set_max_open_files(256);
    DB::open_cf_for_read_only(opts, path, cfs, false).map_err(|e| anyhow!("open {path}: {e}"))
}
