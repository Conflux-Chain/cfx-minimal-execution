// Scan blockchain_db raw block headers, filter by header nonce > 2^128, group
// by author address, and dump each author's sorted block timestamps. Downstream
// (Python) tests each author's inter-block interval sequence for departure from
// an exponential distribution (non-Poisson mining cadence).
//
// Reuses the cfxpack extractor's raw-RLP header-read approach (see dump_epoch):
// fields before pow_hash(14) are read by index and are NOT affected by the
// stored-header pow_hash shift, and this avoids BlockHeader::decode_with_pow_hash
// which panics on some headers.
//
// Header RLP layout (stream_rlp): 0 parent_hash, 1 height, 2 timestamp,
// 3 author, 4 transactions_root, 5 deferred_state_root, 6 deferred_receipts_root,
// 7 deferred_logs_bloom_hash, 8 blame, 9 difficulty, 10 adaptive, 11 gas_limit,
// 12 referee_hashes, 13 nonce, [14 pow_hash in stored form], ...
//
// Usage:
//   nonce_addr_stats probe <blockchain_db>
//   nonce_addr_stats scan  <blockchain_db> <start_epoch> <end_epoch> <out_file> [threads]
use anyhow::{anyhow, Context, Result};
use cfx_types::{Address, H256, U256};
use rlp::Rlp;
use rocksdb::{
    rocksdb_options::ColumnFamilyDescriptor, BlockBasedOptions, Cache, ColumnFamilyOptions,
    DBOptions, LRUCacheOptions, ReadOptions, DB,
};
use std::collections::HashMap;
use std::env;
use std::sync::Arc;
use std::thread;

const COL_BLOCKS: &str = "col1";
const COL_EPOCH_NUMBERS: &str = "col3";
const EXECUTED_SUFFIX: u8 = 0x06;

struct DbWrap(DB);
// Read-only concurrent gets are safe; mirror db.rs's PowDb Send/Sync.
unsafe impl Send for DbWrap {}
unsafe impl Sync for DbWrap {}

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    let mode = args.get(1).map(|s| s.as_str()).unwrap_or("");
    match mode {
        "probe" => {
            let db = open_db(args.get(2).ok_or_else(|| anyhow!("need <db>"))?)?;
            probe(&db)
        }
        "scan" => {
            if args.len() < 6 {
                return Err(anyhow!(
                    "Usage: nonce_addr_stats scan <db> <start_epoch> <end_epoch> <out_file> [threads]"
                ));
            }
            let db = Arc::new(DbWrap(open_db(&args[2])?));
            let start: u64 = args[3].parse().context("start_epoch")?;
            let end: u64 = args[4].parse().context("end_epoch")?;
            let out = &args[5];
            let threads: usize = args.get(6).map(|s| s.parse()).transpose()?.unwrap_or(24);
            scan(db, start, end, out, threads)
        }
        "scanmonth" => {
            if args.len() < 5 {
                return Err(anyhow!(
                    "Usage: nonce_addr_stats scanmonth <db> <days> <out_file> [threads]"
                ));
            }
            let db = Arc::new(DbWrap(open_db(&args[2])?));
            let days: u64 = args[3].parse().context("days")?;
            let out = &args[4];
            let threads: usize = args.get(5).map(|s| s.parse()).transpose()?.unwrap_or(24);
            let tip = find_tip(&db.0)?;
            let tip_ts = epoch_pivot_timestamp(&db.0, tip)?
                .ok_or_else(|| anyhow!("no tip timestamp"))?;
            let target = tip_ts.saturating_sub(days * 86400);
            let start = find_start_epoch(&db.0, target, tip)?;
            eprintln!(
                "tip_epoch={tip} tip_ts={tip_ts} window_start_ts={target} start_epoch={start} epochs={}",
                tip - start + 1
            );
            scan(db, start, tip, out, threads)
        }
        "hashmonth" => {
            if args.len() < 5 {
                return Err(anyhow!(
                    "Usage: nonce_addr_stats hashmonth <db> <days> <out_file> [threads]"
                ));
            }
            let db = Arc::new(DbWrap(open_db(&args[2])?));
            let days: u64 = args[3].parse().context("days")?;
            let out = &args[4];
            let threads: usize = args.get(5).map(|s| s.parse()).transpose()?.unwrap_or(24);
            let tip = find_tip(&db.0)?;
            let tip_ts = epoch_pivot_timestamp(&db.0, tip)?
                .ok_or_else(|| anyhow!("no tip timestamp"))?;
            let target = tip_ts.saturating_sub(days * 86400);
            let start = find_start_epoch(&db.0, target, tip)?;
            eprintln!(
                "tip_epoch={tip} tip_ts={tip_ts} window_start_ts={target} start_epoch={start} epochs={}",
                tip - start + 1
            );
            scan_hashrate(db, start, tip, out, threads)
        }
        "dumpnonce" => {
            // dumpnonce <db> <start_epoch> <end_epoch> <out>
            // Dump ALL executed blocks: author, full nonce, pow_hash(H), difficulty.
            // RLP stored layout: difficulty=9, nonce=13, pow_hash=14.
            let db = open_db(&args[2])?;
            let start: u64 = args[3].parse().context("start")?;
            let end: u64 = args[4].parse().context("end")?;
            let out = &args[5];
            use std::io::Write;
            let mut w = std::io::BufWriter::new(std::fs::File::create(out)?);
            writeln!(w, "epoch\tauthor\tnonce_hex\tpow_hash\tdifficulty")?;
            let mut n = 0u64;
            for epoch in start..=end {
                let Some(hashes) = read_epoch_hashes(&db, epoch, EXECUTED_SUFFIX)? else {
                    continue;
                };
                for h in &hashes {
                    let Some(bytes) = read_header_bytes(&db, h)? else { continue };
                    let rlp = Rlp::new(&bytes);
                    let author = match rlp.val_at::<Address>(3) { Ok(a) => a, _ => continue };
                    let nonce = match rlp.val_at::<U256>(13) { Ok(n) => n, _ => continue };
                    let difficulty = match rlp.val_at::<U256>(9) { Ok(d) => d, _ => continue };
                    // index 14 is Option<H256> (wire: a string wrapping rlp(H256)).
                    let powh = rlp
                        .val_at::<Option<H256>>(14)
                        .ok()
                        .flatten()
                        .unwrap_or_else(H256::zero);
                    writeln!(w, "{epoch}\t{author:x}\t{nonce:x}\t{powh:x}\t{difficulty:x}")?;
                    n += 1;
                }
            }
            w.flush()?;
            eprintln!("dumped {n} blocks -> {out}");
            Ok(())
        }
        "history" => {
            // history <db> <step_days> <window_days> <out> [threads]
            let db = Arc::new(DbWrap(open_db(&args[2])?));
            let step_days: u64 = args[3].parse().context("step_days")?;
            let window_days: u64 = args[4].parse().context("window_days")?;
            let out = &args[5];
            let threads: usize = args.get(6).map(|s| s.parse()).transpose()?.unwrap_or(24);
            history(db, step_days, window_days, threads, out)
        }
        "scanfwd" => {
            // scanfwd <db> <offset_days> <window_days> <out> [threads]
            // Per-high-nonce-author timestamps over a forward window from genesis.
            let db = Arc::new(DbWrap(open_db(&args[2])?));
            let offset_days: u64 = args[3].parse().context("offset_days")?;
            let window_days: u64 = args[4].parse().context("window_days")?;
            let out = &args[5];
            let threads: usize = args.get(6).map(|s| s.parse()).transpose()?.unwrap_or(24);
            let tip = find_tip(&db.0)?;
            let mut genesis_ts = 0u64;
            for e in 1..=50u64 {
                if let Some(ts) = epoch_pivot_timestamp(&db.0, e)? {
                    genesis_ts = ts;
                    break;
                }
            }
            let lo = genesis_ts + offset_days * 86400;
            let hi = lo + window_days * 86400;
            let e_lo = find_start_epoch(&db.0, lo, tip)?;
            let e_hi = find_start_epoch(&db.0, hi, tip)?;
            eprintln!("genesis_ts={genesis_ts} window[{lo}..{hi}] epochs[{}..{e_hi}]", e_lo + 1);
            scan(db, e_lo + 1, e_hi, out, threads)
        }
        "histfwd" => {
            // histfwd <db> <offset_days> <step_days> <window_days> <n_samples> <out> [threads]
            // Forward from genesis+offset; per sample print high-nonce group's
            // hashpower-share distribution: p10/p50/p90/mean/std.
            let db = Arc::new(DbWrap(open_db(&args[2])?));
            let offset_days: u64 = args[3].parse().context("offset_days")?;
            let step_days: u64 = args[4].parse().context("step_days")?;
            let window_days: u64 = args[5].parse().context("window_days")?;
            let n_samples: u64 = args[6].parse().context("n_samples")?;
            let out = &args[7];
            let threads: usize = args.get(8).map(|s| s.parse()).transpose()?.unwrap_or(24);
            history_forward(db, offset_days, step_days, window_days, n_samples, threads, out)
        }
        "epochdur" => {
            // epochdur <db> <days> [threads]
            // Finalized-epoch duration = delta between consecutive pivot-block
            // timestamps, over the last <days>. Reports mean/p50/p90/p99/max.
            let db = Arc::new(DbWrap(open_db(&args[2])?));
            let days: u64 = args[3].parse().context("days")?;
            let threads: usize = args.get(4).map(|s| s.parse()).transpose()?.unwrap_or(24);
            epoch_durations(db, days, threads)
        }
        "epochwin" => {
            // epochwin <db> <ts_lo> <ts_hi> : print "e_lo e_hi" bracketing the
            // pivot-timestamp window [ts_lo, ts_hi].
            let db = open_db(&args[2])?;
            let ts_lo: u64 = args[3].parse().context("ts_lo")?;
            let ts_hi: u64 = args[4].parse().context("ts_hi")?;
            let tip = find_tip(&db)?;
            let e_lo = find_start_epoch(&db, ts_lo, tip)? + 1;
            let e_hi = find_start_epoch(&db, ts_hi, tip)?;
            println!("{e_lo} {e_hi}");
            Ok(())
        }
        "pivotcurve" => {
            // pivotcurve <db> <e_lo> <e_hi> <step>
            // Authoritative epoch<->pivot-timestamp curve straight from
            // blockchain_db, to measure local epoch production rate.
            let db = open_db(&args[2])?;
            let e_lo: u64 = args[3].parse().context("e_lo")?;
            let e_hi: u64 = args[4].parse().context("e_hi")?;
            let step: u64 = args.get(5).map(|s| s.parse()).transpose()?.unwrap_or(1);
            println!("epoch\tpivot_ts\td_epoch\td_ts\tep_per_s");
            let mut prev: Option<(u64, u64)> = None;
            let mut e = e_lo;
            while e <= e_hi {
                if let Some(ts) = epoch_pivot_timestamp(&db, e)? {
                    let (de, dt, rate) = match prev {
                        Some((pe, pts)) => {
                            let de = e - pe;
                            let dt = ts as i64 - pts as i64;
                            let rate = if dt != 0 { de as f64 / dt as f64 } else { f64::INFINITY };
                            (de, dt, rate)
                        }
                        None => (0, 0, 0.0),
                    };
                    println!("{e}\t{ts}\t{de}\t{dt}\t{rate:.3}");
                    prev = Some((e, ts));
                }
                e += step;
            }
            Ok(())
        }
        "headers" => {
            // headers <db> <hash_list_tsv> <out>
            // hash_list_tsv: first column = block hash hex (header line allowed).
            // Dumps per-block header fields for structure/author/nonce analysis.
            let db = open_db(&args[2])?;
            let list = std::fs::read_to_string(&args[3])?;
            let out = &args[4];
            use std::io::Write;
            let mut w = std::io::BufWriter::new(std::fs::File::create(out)?);
            writeln!(
                w,
                "hash\tparent\theight\tts\tauthor\tblame\tdifficulty\tadaptive\tn_ref\tnonce_hex\treferees"
            )?;
            let (mut n, mut miss) = (0u64, 0u64);
            for line in list.lines() {
                let Some(hs) = line.split('\t').next() else { continue };
                let Ok(hash) = hs.parse::<H256>() else { continue };
                let Some(bytes) = read_header_bytes(&db, &hash)? else {
                    miss += 1;
                    continue;
                };
                let rlp = Rlp::new(&bytes);
                let parent = rlp.val_at::<H256>(0)?;
                let height = rlp.val_at::<u64>(1)?;
                let ts = rlp.val_at::<u64>(2)?;
                let author = rlp.val_at::<Address>(3)?;
                let blame = rlp.val_at::<u64>(8)?;
                let difficulty = rlp.val_at::<U256>(9)?;
                let adaptive = rlp.val_at::<bool>(10)?;
                let referees = rlp.list_at::<H256>(12)?;
                let nonce = rlp.val_at::<U256>(13)?;
                let refs: Vec<String> = referees.iter().map(|r| format!("{r:x}")).collect();
                writeln!(
                    w,
                    "{hash:x}\t{parent:x}\t{height}\t{ts}\t{author:x}\t{blame}\t{difficulty}\t{}\t{}\t{nonce:x}\t{}",
                    adaptive as u8,
                    referees.len(),
                    refs.join(",")
                )?;
                n += 1;
            }
            w.flush()?;
            eprintln!("headers dumped={n} missing={miss} -> {out}");
            Ok(())
        }
        "diffwin" => {
            // diffwin <db> <e_lo> <e_hi> <ts_lo> <ts_hi>
            // Over epochs [e_lo,e_hi], sum difficulty of executed blocks whose own
            // timestamp falls in [ts_lo,ts_hi] -- the network hashpower baseline
            // for a wall-clock window (blocks may land in much later epochs).
            let db = open_db(&args[2])?;
            let e_lo: u64 = args[3].parse()?;
            let e_hi: u64 = args[4].parse()?;
            let ts_lo: u64 = args[5].parse()?;
            let ts_hi: u64 = args[6].parse()?;
            let mut n = 0u64;
            let mut sum = U256::zero();
            let mut d_min = U256::max_value();
            let mut d_max = U256::zero();
            for epoch in e_lo..=e_hi {
                let Some(hashes) = read_epoch_hashes(&db, epoch, EXECUTED_SUFFIX)? else {
                    continue;
                };
                for h in &hashes {
                    let Some(bytes) = read_header_bytes(&db, h)? else { continue };
                    let rlp = Rlp::new(&bytes);
                    let ts = rlp.val_at::<u64>(2)?;
                    if ts < ts_lo || ts > ts_hi {
                        continue;
                    }
                    let d = rlp.val_at::<U256>(9)?;
                    n += 1;
                    sum += d;
                    d_min = d_min.min(d);
                    d_max = d_max.max(d);
                }
            }
            println!("epochs[{e_lo}..{e_hi}] ts[{ts_lo}..{ts_hi}] n_blocks={n} diff_sum={sum} diff_min={d_min} diff_max={d_max}");
            Ok(())
        }
        "timerwin" => {
            // timerwin <db> <e_lo> <e_hi> <ts_lo> <ts_hi>
            // Over epochs [e_lo,e_hi], for executed blocks whose own ts is in
            // [ts_lo,ts_hi]: split by nonce>2^128 (wide) vs not, and count timer
            // blocks per pow_hash_to_quality >= 180*difficulty. Tests whether a
            // delayed-release matches timer_chain_beta=240 timer blocks.
            let db = open_db(&args[2])?;
            let e_lo: u64 = args[3].parse()?;
            let e_hi: u64 = args[4].parse()?;
            let ts_lo: u64 = args[5].parse()?;
            let ts_hi: u64 = args[6].parse()?;
            let wide_thr = U256::from(1) << 128u32;
            // pow_hash_to_quality (cfxcore/pow): lower_bound = nonce's high 16
            // bytes with top bit masked; quality = 2^256/(hash - lower_bound + 1).
            let quality = |hash: &H256, nonce: &U256| -> U256 {
                let mut buf = nonce.to_big_endian();
                for b in buf[16..32].iter_mut() {
                    *b = 0;
                }
                buf[0] &= 0x7f;
                let lower = U256::from_big_endian(&buf);
                let h = U256::from_big_endian(hash.as_bytes());
                let (against, _) = h.overflowing_sub(lower);
                if against == U256::max_value() {
                    return U256::one();
                }
                let x = against + U256::one();
                if x == U256::one() {
                    return U256::max_value();
                }
                let (div, m) = U256::max_value().div_mod(x);
                if m + U256::one() == x {
                    div + U256::one()
                } else {
                    div
                }
            };
            let (mut n_all, mut n_wide) = (0u64, 0u64);
            let (mut d_all, mut d_wide) = (U256::zero(), U256::zero());
            let (mut t_all, mut t_wide) = (0u64, 0u64);
            let mut no_powhash = 0u64;
            for epoch in e_lo..=e_hi {
                let Some(hashes) = read_epoch_hashes(&db, epoch, EXECUTED_SUFFIX)? else {
                    continue;
                };
                for h in &hashes {
                    let Some(bytes) = read_header_bytes(&db, h)? else { continue };
                    let rlp = Rlp::new(&bytes);
                    let ts = rlp.val_at::<u64>(2)?;
                    if ts < ts_lo || ts > ts_hi {
                        continue;
                    }
                    let d = rlp.val_at::<U256>(9)?;
                    let nonce = rlp.val_at::<U256>(13)?;
                    let Some(powh) = rlp.val_at::<Option<H256>>(14).ok().flatten() else {
                        no_powhash += 1;
                        continue;
                    };
                    let q = quality(&powh, &nonce);
                    let is_timer = q >= d * U256::from(180u64);
                    let wide = nonce > wide_thr;
                    n_all += 1;
                    d_all += d;
                    if is_timer {
                        t_all += 1;
                    }
                    if wide {
                        n_wide += 1;
                        d_wide += d;
                        if is_timer {
                            t_wide += 1;
                        }
                    }
                }
            }
            println!(
                "epochs[{e_lo}..{e_hi}] ts[{ts_lo}..{ts_hi}] span={}s\n\
                 all : n={n_all} diff_sum={d_all} timer={t_all}\n\
                 wide: n={n_wide} diff_sum={d_wide} timer={t_wide}\n\
                 nonwide_timer={} no_powhash={no_powhash}",
                ts_hi - ts_lo,
                t_all - t_wide
            );
            Ok(())
        }
        "rlpinfo" => {
            // rlpinfo <db> <epoch> : dump RLP item count + each item's byte length/hex for blocks
            let db = open_db(&args[2])?;
            let epoch: u64 = args[3].parse()?;
            let hashes = read_epoch_hashes(&db, epoch, EXECUTED_SUFFIX)?.unwrap_or_default();
            for h in hashes.iter().take(4) {
                let Some(bytes) = read_header_bytes(&db, h)? else { continue };
                let r = Rlp::new(&bytes);
                let cnt = r.item_count().unwrap_or(0);
                println!("block {h:x}  item_count={cnt}");
                for i in 0..cnt {
                    let it = r.at(i).unwrap();
                    let raw = it.data().unwrap_or(&[]);
                    let hex: String = raw.iter().take(34).map(|b| format!("{b:02x}")).collect();
                    println!("  [{i:>2}] len={:>3}  {hex}", raw.len());
                }
                println!();
            }
            Ok(())
        }
        _ => Err(anyhow!(
            "Usage:\n  nonce_addr_stats probe <db>\n  nonce_addr_stats scan <db> <start> <end> <out> [threads]\n  nonce_addr_stats scanmonth <db> <days> <out> [threads]\n  nonce_addr_stats hashmonth <db> <days> <out> [threads]\n  nonce_addr_stats dumpnonce <db> <start> <end> <out>\n  nonce_addr_stats rlpinfo <db> <epoch>"
        )),
    }
}

/// Per-author block counts (total + high-nonce) over an epoch range = hashpower.
/// Parallel per-author [total, high_nonce] counts over [start,end]. Shared by
/// scan_hashrate and history.
fn collect_authors(
    db: &Arc<DbWrap>,
    start: u64,
    end: u64,
    threads: usize,
) -> Result<(HashMap<Address, [u64; 2]>, u64)> {
    let threshold = U256::from(1) << 128u32;
    let total = end.saturating_sub(start) + 1;
    let chunk = total.div_ceil(threads as u64).max(1);
    let mut handles = Vec::new();
    for t in 0..threads as u64 {
        let s = start + t * chunk;
        if s > end {
            break;
        }
        let e = (s + chunk - 1).min(end);
        let db = Arc::clone(db);
        handles.push(thread::spawn(move || worker_hashrate(&db.0, s, e, threshold)));
    }
    let mut acc: HashMap<Address, [u64; 2]> = HashMap::new();
    let mut grand_total: u64 = 0;
    for h in handles {
        let (m, tot) = h.join().map_err(|_| anyhow!("worker panicked"))??;
        grand_total += tot;
        for (a, c) in m {
            let e = acc.entry(a).or_default();
            e[0] += c[0];
            e[1] += c[1];
        }
    }
    Ok((acc, grand_total))
}

/// Sample `window_days`-day windows every `step_days`, walking back from tip, and
/// report the high-nonce share / author count / hashpower uniformity in each —
/// so we can see WHEN the high-nonce anomaly first appears and whether it's stable.
fn history(
    db: Arc<DbWrap>,
    step_days: u64,
    window_days: u64,
    threads: usize,
    out: &str,
) -> Result<()> {
    use std::io::Write;
    let tip = find_tip(&db.0)?;
    let tip_ts = epoch_pivot_timestamp(&db.0, tip)?.ok_or_else(|| anyhow!("no tip ts"))?;
    let f = std::fs::File::create(out)?;
    let mut w = std::io::BufWriter::new(f);
    writeln!(
        w,
        "center_ts\te_lo\te_hi\ttotal\thigh\thigh_pct\tn_authors\tn_high_authors\thi_share_min\thi_share_med\thi_share_max"
    )?;
    println!("center_ts    e_lo..e_hi              total   high  high%  n_auth n_hiA  hi_share[min/med/max %]");
    let mut k: u64 = 0;
    loop {
        let center = tip_ts as i64 - (k * step_days * 86400) as i64;
        if center < 0 {
            break;
        }
        let center = center as u64;
        let lo_ts = center.saturating_sub(window_days * 86400);
        let e_hi = find_start_epoch(&db.0, center, tip)?;
        let e_lo = find_start_epoch(&db.0, lo_ts, tip)?;
        if e_hi <= 2 || e_hi <= e_lo {
            // reached genesis region
            break;
        }
        let (acc, total) = collect_authors(&db, e_lo + 1, e_hi, threads)?;
        let high: u64 = acc.values().map(|c| c[1]).sum();
        let n_auth = acc.len();
        // hashpower share (of window total) for high-nonce authors
        let mut hi_shares: Vec<f64> = acc
            .values()
            .filter(|c| c[1] > 0)
            .map(|c| c[0] as f64 / total.max(1) as f64 * 100.0)
            .collect();
        hi_shares.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let n_hi = hi_shares.len();
        let (smin, smed, smax) = if n_hi > 0 {
            (hi_shares[0], hi_shares[n_hi / 2], hi_shares[n_hi - 1])
        } else {
            (0.0, 0.0, 0.0)
        };
        let high_pct = high as f64 / total.max(1) as f64 * 100.0;
        writeln!(
            w,
            "{center}\t{}\t{e_hi}\t{total}\t{high}\t{high_pct:.3}\t{n_auth}\t{n_hi}\t{smin:.4}\t{smed:.4}\t{smax:.4}",
            e_lo + 1
        )?;
        w.flush()?;
        println!(
            "{center}  {:>9}..{e_hi:<9}  {total:>7} {high:>6} {high_pct:>5.2}  {n_auth:>5} {n_hi:>5}  {smin:.4}/{smed:.4}/{smax:.4}",
            e_lo + 1
        );
        k += 1;
    }
    eprintln!("history done -> {out} ({} samples)", k);
    Ok(())
}

fn pctl(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = (q * (sorted.len() as f64 - 1.0)).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// Forward sampling from the earliest epoch: every `step_days`, a `window_days`
/// window, for `n_samples` points. Reports the high-nonce group's per-address
/// hashpower-share distribution (p10/p50/p90/mean/std) in each window.
fn history_forward(
    db: Arc<DbWrap>,
    offset_days: u64,
    step_days: u64,
    window_days: u64,
    n_samples: u64,
    threads: usize,
    out: &str,
) -> Result<()> {
    use std::io::Write;
    let tip = find_tip(&db.0)?;
    // earliest readable pivot timestamp (epoch 1 or nearby)
    let mut genesis_ts = 0u64;
    for e in 1..=50u64 {
        if let Some(ts) = epoch_pivot_timestamp(&db.0, e)? {
            genesis_ts = ts;
            break;
        }
    }
    if genesis_ts == 0 {
        return Err(anyhow!("no genesis timestamp"));
    }
    let f = std::fs::File::create(out)?;
    let mut w = std::io::BufWriter::new(f);
    writeln!(
        w,
        "center_ts\te_lo\te_hi\ttotal\thigh\thigh_pct\tn_auth\tn_high_authors\tp10\tp50\tp90\tmean\tstd"
    )?;
    println!("date(UTC)   e_lo..e_hi            total   high  high%  n_auth n_hiA   p10    p50    p90    mean   std   (乙组算力share%)");
    for k in 0..n_samples {
        let center = genesis_ts + (offset_days + k * step_days) * 86400;
        let e_lo = find_start_epoch(&db.0, center, tip)?;
        let e_hi = find_start_epoch(&db.0, center + window_days * 86400, tip)?;
        if e_hi <= e_lo {
            break;
        }
        let (acc, total) = collect_authors(&db, e_lo + 1, e_hi, threads)?;
        let n_auth = acc.len();
        let high: u64 = acc.values().map(|c| c[1]).sum();
        let mut shares: Vec<f64> = acc
            .values()
            .filter(|c| c[1] > 0)
            .map(|c| c[0] as f64 / total.max(1) as f64 * 100.0)
            .collect();
        shares.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let n = shares.len();
        let mean = if n > 0 { shares.iter().sum::<f64>() / n as f64 } else { 0.0 };
        let var = if n > 1 {
            shares.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n as f64 - 1.0)
        } else {
            0.0
        };
        let std = var.sqrt();
        let (p10, p50, p90) = (pctl(&shares, 0.10), pctl(&shares, 0.50), pctl(&shares, 0.90));
        let high_pct = high as f64 / total.max(1) as f64 * 100.0;
        writeln!(
            w,
            "{center}\t{}\t{e_hi}\t{total}\t{high}\t{high_pct:.3}\t{n_auth}\t{n}\t{p10:.5}\t{p50:.5}\t{p90:.5}\t{mean:.5}\t{std:.5}",
            e_lo + 1
        )?;
        w.flush()?;
        println!(
            "{center}  {:>9}..{e_hi:<9} {total:>7} {high:>6} {high_pct:>5.2}  {n_auth:>5} {n:>5}  {p10:.4} {p50:.4} {p90:.4} {mean:.4} {std:.4}",
            e_lo + 1
        );
    }
    eprintln!("histfwd done -> {out}");
    Ok(())
}

fn scan_hashrate(db: Arc<DbWrap>, start: u64, end: u64, out: &str, threads: usize) -> Result<()> {
    let (acc, grand_total) = collect_authors(&db, start, end, threads)?;
    use std::io::Write;
    let f = std::fs::File::create(out).with_context(|| format!("create {out}"))?;
    let mut w = std::io::BufWriter::new(f);
    // sort by total blocks desc
    let mut rows: Vec<_> = acc.into_iter().collect();
    rows.sort_by(|a, b| b.1[0].cmp(&a.1[0]).then(a.0.cmp(&b.0)));
    writeln!(w, "# grand_total_blocks={grand_total}")?;
    writeln!(w, "addr\ttotal\thigh_nonce\tlow_nonce\tshare_pct")?;
    for (addr, c) in &rows {
        let share = c[0] as f64 / grand_total as f64 * 100.0;
        writeln!(w, "{:x}\t{}\t{}\t{}\t{:.5}", addr, c[0], c[1], c[0] - c[1], share)?;
    }
    w.flush()?;
    eprintln!(
        "grand_total_blocks={grand_total} n_authors={} (sorted by total desc -> {out})",
        rows.len()
    );
    Ok(())
}

fn worker_hashrate(
    db: &DB,
    start: u64,
    end: u64,
    threshold: U256,
) -> Result<(HashMap<Address, [u64; 2]>, u64)> {
    let mut map: HashMap<Address, [u64; 2]> = HashMap::new();
    let mut total = 0u64;
    for epoch in start..=end {
        let Some(hashes) = read_epoch_hashes(db, epoch, EXECUTED_SUFFIX)? else {
            continue;
        };
        for h in &hashes {
            let Some(bytes) = read_header_bytes(db, h)? else {
                continue;
            };
            let rlp = Rlp::new(&bytes);
            let (author, nonce) = match (rlp.val_at::<Address>(3), rlp.val_at::<U256>(13)) {
                (Ok(a), Ok(n)) => (a, n),
                _ => continue,
            };
            total += 1;
            let e = map.entry(author).or_default();
            e[0] += 1;
            if nonce > threshold {
                e[1] += 1;
            }
        }
    }
    Ok((map, total))
}

/// Exponential-jump then binary-search for the largest epoch that still has an
/// executed block set.
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

/// Largest epoch in [1, tip] whose pivot timestamp <= target_ts. Timestamps are
/// miner-set and not strictly monotone, but the pivot chain is monotone enough
/// to bracket a window edge; the Python side re-filters by exact timestamp.
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

fn probe(db: &DB) -> Result<()> {
    let tip = find_tip(db)?;
    let ts = epoch_pivot_timestamp(db, tip)?;
    println!("tip_epoch={tip} pivot_timestamp={ts:?}");
    if let Some(ts) = ts {
        // Human-readable UTC (no chrono dep; compute manually).
        println!("tip_timestamp_unix={ts}");
        println!("one_month_ago_unix={}", ts.saturating_sub(30 * 86400));
    }
    Ok(())
}

fn epoch_pivot_timestamp(db: &DB, epoch: u64) -> Result<Option<u64>> {
    let Some(hashes) = read_epoch_hashes(db, epoch, EXECUTED_SUFFIX)? else {
        return Ok(None);
    };
    let Some(pivot) = hashes.last() else {
        return Ok(None);
    };
    let bytes = read_header_bytes(db, pivot)?.ok_or_else(|| anyhow!("missing pivot header"))?;
    let rlp = Rlp::new(&bytes);
    Ok(Some(rlp.val_at::<u64>(2).context("timestamp")?))
}

/// Finalized-epoch durations over the last `days`. Reads every executed epoch's
/// pivot-block timestamp in parallel, then measures the gap to the previous
/// epoch's pivot. Timestamps are miner-set at 1s resolution and not strictly
/// monotone, so backward gaps are clamped to 0 (and counted).
fn epoch_durations(db: Arc<DbWrap>, days: u64, threads: usize) -> Result<()> {
    const MISSING: u64 = u64::MAX;
    let tip = find_tip(&db.0)?;
    let tip_ts = epoch_pivot_timestamp(&db.0, tip)?.ok_or_else(|| anyhow!("no tip ts"))?;
    let start_ts = tip_ts.saturating_sub(days * 86400);
    let e_start = find_start_epoch(&db.0, start_ts, tip)?;
    let total = tip - e_start + 1;
    let chunk = total.div_ceil(threads as u64);
    let mut handles = Vec::new();
    for t in 0..threads as u64 {
        let s = e_start + t * chunk;
        if s > tip {
            break;
        }
        let e = (s + chunk - 1).min(tip);
        let db = Arc::clone(&db);
        handles.push(thread::spawn(move || -> Result<Vec<u64>> {
            let mut v = Vec::with_capacity((e - s + 1) as usize);
            for epoch in s..=e {
                v.push(epoch_pivot_timestamp(&db.0, epoch)?.unwrap_or(MISSING));
            }
            Ok(v)
        }));
    }
    // Chunks are contiguous and ordered by t -> concatenation is epoch order.
    let mut ts: Vec<u64> = Vec::with_capacity(total as usize);
    for h in handles {
        ts.extend(h.join().map_err(|_| anyhow!("worker panicked"))??);
    }

    let mut deltas: Vec<f64> = Vec::with_capacity(ts.len());
    let mut negatives = 0u64;
    let mut missing = 0u64;
    for w in ts.windows(2) {
        let (prev, cur) = (w[0], w[1]);
        if prev == MISSING || cur == MISSING {
            missing += 1;
            continue;
        }
        let d = cur as i64 - prev as i64;
        if d < 0 {
            negatives += 1;
            deltas.push(0.0);
        } else {
            deltas.push(d as f64);
        }
    }
    deltas.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap());
    let n = deltas.len();
    let sum: f64 = deltas.iter().sum();
    // Span-based mean is robust to the 1s quantization and non-monotonicity.
    let (first_ts, last_ts) = (
        ts.iter().copied().find(|&x| x != MISSING),
        ts.iter().rev().copied().find(|&x| x != MISSING),
    );
    let span_mean = match (first_ts, last_ts) {
        (Some(a), Some(b)) if n > 0 => (b.saturating_sub(a)) as f64 / n as f64,
        _ => f64::NAN,
    };
    println!("=== finalized epoch duration | last {days}d ===");
    println!("epochs[{e_start}..{tip}]  n_epochs={total}  n_intervals={n}  missing={missing}");
    println!(
        "span={:.2}d  window_ts[{}..{}]",
        (last_ts.unwrap_or(0).saturating_sub(first_ts.unwrap_or(0))) as f64 / 86400.0,
        first_ts.unwrap_or(0),
        last_ts.unwrap_or(0),
    );
    println!("mean(span/n)={span_mean:.4}s  mean(deltas)={:.4}s", sum / n.max(1) as f64);
    println!(
        "p50={:.0}s  p90={:.0}s  p99={:.0}s  p999={:.0}s  max={:.0}s",
        pctl(&deltas, 0.50),
        pctl(&deltas, 0.90),
        pctl(&deltas, 0.99),
        pctl(&deltas, 0.999),
        deltas.last().copied().unwrap_or(0.0),
    );
    println!(
        "non-monotone(backward) intervals={} ({:.3}%)",
        negatives,
        100.0 * negatives as f64 / n.max(1) as f64,
    );
    Ok(())
}

struct Partial {
    // author -> timestamps
    by_addr: HashMap<Address, Vec<u64>>,
    scanned_blocks: u64,
    filtered_blocks: u64, // nonce > 2^128 (kept)
    small_nonce: u64,     // nonce <= 2^128 (dropped)
    decode_err: u64,
    ts_min: u64,
    ts_max: u64,
}

impl Partial {
    fn new() -> Self {
        Self {
            by_addr: HashMap::new(),
            scanned_blocks: 0,
            filtered_blocks: 0,
            small_nonce: 0,
            decode_err: 0,
            ts_min: u64::MAX,
            ts_max: 0,
        }
    }
    fn merge(&mut self, o: Partial) {
        for (a, mut v) in o.by_addr {
            self.by_addr.entry(a).or_default().append(&mut v);
        }
        self.scanned_blocks += o.scanned_blocks;
        self.filtered_blocks += o.filtered_blocks;
        self.small_nonce += o.small_nonce;
        self.decode_err += o.decode_err;
        self.ts_min = self.ts_min.min(o.ts_min);
        self.ts_max = self.ts_max.max(o.ts_max);
    }
}

fn scan(db: Arc<DbWrap>, start: u64, end: u64, out: &str, threads: usize) -> Result<()> {
    let threshold = U256::from(1) << 128u32;
    let total = end.saturating_sub(start) + 1;
    let chunk = total.div_ceil(threads as u64);
    let mut handles = Vec::new();
    for t in 0..threads as u64 {
        let s = start + t * chunk;
        if s > end {
            break;
        }
        let e = (s + chunk - 1).min(end);
        let db = Arc::clone(&db);
        handles.push(thread::spawn(move || worker(&db.0, s, e, threshold)));
    }
    let mut acc = Partial::new();
    for h in handles {
        let p = h.join().map_err(|_| anyhow!("worker panicked"))?;
        acc.merge(p?);
    }

    // Sort each author's timestamps, write out.
    use std::io::Write;
    let f = std::fs::File::create(out).with_context(|| format!("create {out}"))?;
    let mut w = std::io::BufWriter::new(f);
    let mut addrs: Vec<_> = acc.by_addr.into_iter().collect();
    // Deterministic order: by descending block count.
    addrs.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(&b.0)));
    for (addr, ts) in addrs.iter_mut() {
        ts.sort_unstable();
        write!(w, "{:x}\t{}\t", addr, ts.len())?;
        for (i, t) in ts.iter().enumerate() {
            if i > 0 {
                w.write_all(b",")?;
            }
            write!(w, "{t}")?;
        }
        w.write_all(b"\n")?;
    }
    w.flush()?;

    eprintln!(
        "scanned_epochs={} scanned_blocks={} kept(nonce>2^128)={} dropped(nonce<=2^128)={} decode_err={} n_addresses={} ts_min={} ts_max={} span_days={:.2}",
        total,
        acc.scanned_blocks,
        acc.filtered_blocks,
        acc.small_nonce,
        acc.decode_err,
        addrs.len(),
        acc.ts_min,
        acc.ts_max,
        (acc.ts_max.saturating_sub(acc.ts_min)) as f64 / 86400.0,
    );
    Ok(())
}

fn worker(db: &DB, start: u64, end: u64, threshold: U256) -> Result<Partial> {
    let mut p = Partial::new();
    for epoch in start..=end {
        let Some(hashes) = read_epoch_hashes(db, epoch, EXECUTED_SUFFIX)? else {
            continue;
        };
        for h in &hashes {
            let Some(bytes) = read_header_bytes(db, h)? else {
                p.decode_err += 1;
                continue;
            };
            p.scanned_blocks += 1;
            let rlp = Rlp::new(&bytes);
            let (ts, author, nonce) = match (
                rlp.val_at::<u64>(2),
                rlp.val_at::<Address>(3),
                rlp.val_at::<U256>(13),
            ) {
                (Ok(ts), Ok(a), Ok(n)) => (ts, a, n),
                _ => {
                    p.decode_err += 1;
                    continue;
                }
            };
            if nonce > threshold {
                p.filtered_blocks += 1;
                p.by_addr.entry(author).or_default().push(ts);
                p.ts_min = p.ts_min.min(ts);
                p.ts_max = p.ts_max.max(ts);
            } else {
                p.small_nonce += 1;
            }
        }
    }
    Ok(p)
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
        .map_err(|e| anyhow!("read col1 header: {e}"))
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
