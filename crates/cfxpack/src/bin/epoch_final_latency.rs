//! Finalization latency of finalized epochs, read straight from cfxpack.
//!
//! Each packed `Block` carries `finalized_epoch` as an OFFSET: the extractor
//! stores `block.epoch - pivot_decision_height`, so the absolute deepest
//! finalized epoch known as of a block at epoch C is `fin_abs = C - offset`
//! (see extract/mod.rs::finalized_epoch_offset).
//!
//! For each finalized epoch E we measure how long until it became irreversible:
//!   finalizer = the earliest block (by time) whose `fin_abs >= E`
//!   lag_time(E)   = finalizer.timestamp - pivot_ts(E)          [seconds]
//!   lag_epochs(E) = finalizer.epoch     - E                    [epochs]
//! Both are derived from the running-max staircase of `fin_abs` over time, so
//! the whole thing is O(n log n) (one sort + binary search per epoch), never
//! O(n^2).
//!
//! Usage:
//!   epoch_final_latency <packed_dir> <days>            # single trailing-N-days window
//!   epoch_final_latency <packed_dir> monthly [n_months] [pos_h]
//! The `monthly` mode is self-contained: it derives the last n_months complete
//! UTC+8 calendar months itself and buckets pivots by timestamp -- no external
//! window specs and no blockchain_db. packed_dir is required (no default);
//! pos_h defaults to the mainnet pos_reference_enable_height.

use cfxpack::container;
use cfxpack::decode;
use cfxpack::packet::FLAG_PIVOT;
use rayon::prelude::*;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const DEFAULT_DAYS: u64 = 30;
const DEFAULT_POS_H: u64 = 37_400_000;
// Epochs to over-decode below the target window so every in-window epoch's
// pivot and its finalizer are present (~34 days at mainnet rate).
const DECODE_MARGIN_EPOCHS: u64 = 2_500_000;

// Upper-bound epochs/month, used only to bound how far back `monthly` decodes.
const EPOCHS_PER_MONTH_EST: u64 = 2_600_000;

fn main() -> anyhow::Result<()> {
    if std::env::args().nth(2).as_deref() == Some("monthly") {
        return run_monthly();
    }
    let packed_dir = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("usage: epoch_final_latency <packed_dir> <days>|monthly [n_months] [pos_h]"))?;
    let days: u64 = std::env::args().nth(2).map(|s| s.parse()).transpose()?.unwrap_or(DEFAULT_DAYS);
    let pos_h: u64 =
        std::env::args().nth(3).map(|s| s.parse()).transpose()?.unwrap_or(DEFAULT_POS_H);

    let mut files = collect_files(Path::new(&packed_dir))?;
    files.sort_by_key(|(_, start, _)| *start);
    let max_start = files.last().map(|(_, s, _)| *s).unwrap_or(0);
    let cutoff_start = max_start.saturating_sub(DECODE_MARGIN_EPOCHS);
    let tail: Vec<_> = files.into_iter().filter(|(_, s, _)| *s >= cutoff_start).collect();
    eprintln!(
        "packed_dir={packed_dir} days={days} pos_h={pos_h} | decoding {} tail containers (start>={cutoff_start})",
        tail.len()
    );

    // Collect per block: (epoch, timestamp, fin_abs, is_pivot).
    // fin_abs = epoch - offset; offset (stored finalized_epoch) = confirmation depth.
    let mut blocks: Vec<(u64, u64, u64, bool)> = Vec::new();
    for (path, _start, is_zst) in &tail {
        let raw = read_container(path, *is_zst)?;
        let mut dir = container::parse_directory(&raw)?;
        dir.sort_by_key(|(start, _, _, _)| *start);
        let mut per_group: Vec<Vec<(u64, u64, u64, bool)>> = dir
            .par_iter()
            .map(|(start, count, offset, length)| {
                let payload = &raw[*offset..*offset + *length];
                let packet = match decode::decode_packet_ext(payload, pos_h) {
                    Ok(p) => p,
                    Err(e) => {
                        let end = start + count - 1;
                        eprintln!("decode error group {start}-{end}: {e:#}");
                        return Vec::new();
                    }
                };
                packet
                    .blocks
                    .iter()
                    .map(|b| {
                        let fin_abs = b.epoch.saturating_sub(b.finalized_epoch);
                        (b.epoch, b.timestamp, fin_abs, b.flags & FLAG_PIVOT != 0)
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        for g in per_group.iter_mut() {
            blocks.append(g);
        }
    }
    anyhow::ensure!(!blocks.is_empty(), "no blocks decoded");

    // DIAGNOSTIC: raw stored offset field = per-block confirmation depth
    // (C - fin_abs). offset==0 past PoS activation means "no pos_reference
    // resolved", NOT depth 0 -- flag how common that is and the true depth tail.
    {
        let mut offs: Vec<u64> = Vec::with_capacity(blocks.len());
        let mut zeros = 0u64;
        for &(epoch, _ts, fin_abs, is_pivot) in &blocks {
            if !is_pivot {
                continue; // only pivot blocks define finalization
            }
            let off = epoch.saturating_sub(fin_abs);
            if off == 0 {
                zeros += 1;
            } else {
                offs.push(off);
            }
        }
        offs.sort_unstable();
        let n = offs.len().max(1);
        let mean = offs.iter().sum::<u64>() as f64 / n as f64;
        let q = |f: f64| offs[((f * (offs.len().saturating_sub(1)) as f64).round() as usize).min(offs.len()-1)];
        eprintln!(
            "[diag] raw offset field over PIVOT-only blocks: zeros(no pos_ref)={} nonzero={} | mean={:.1} p50={} p90={} p99={} p999={} max={}",
            zeros, offs.len(), mean,
            q(0.50), q(0.90), q(0.99), q(0.999), offs.last().copied().unwrap_or(0),
        );
    }

    // pivot timestamp per epoch (exactly one pivot per epoch).
    let mut pivot_ts: BTreeMap<u64, u64> = BTreeMap::new();
    let mut max_ts = 0u64;
    for &(epoch, ts, _fin, is_pivot) in &blocks {
        max_ts = max_ts.max(ts);
        if is_pivot {
            pivot_ts.insert(epoch, ts);
        }
    }
    let window_lo_ts = max_ts.saturating_sub(days * 86400);

    // Staircase of finalization progress: sort blocks by time, keep running max
    // of fin_abs. Each strict increase is a step (fin_reached, ts, epoch). Steps
    // are sorted ascending in all three (running max is monotone; ts sorted).
    let mut by_ts = blocks;
    by_ts.sort_unstable_by_key(|&(_e, ts, _f, _p)| ts);
    let mut steps_fin: Vec<u64> = Vec::new(); // fin_reached
    let mut steps_ts: Vec<u64> = Vec::new(); // wall-clock it was reached
    let mut steps_ep: Vec<u64> = Vec::new(); // epoch of the finalizing block
    let mut run_max = 0u64;
    for &(epoch, ts, fin_abs, is_pivot) in &by_ts {
        // Only pivot blocks define finalization; non-pivot (referee) blocks'
        // finalized_epoch does not count (and are the whole large-offset tail).
        if !is_pivot {
            continue;
        }
        // Skip offset==0 blocks (fin_abs==epoch): stored 0 means "no pos_reference
        // resolved", not "own epoch finalized" -- treating it as a real step would
        // spike run_max and manufacture a false finalization jump.
        if fin_abs >= epoch {
            continue;
        }
        if fin_abs > run_max {
            run_max = fin_abs;
            steps_fin.push(run_max);
            steps_ts.push(ts);
            steps_ep.push(epoch);
        }
    }
    let max_finalized = run_max;

    // For each in-window finalized epoch E: binary-search the first step whose
    // fin_reached >= E; that step is E's finalizer.
    let mut lag_time: Vec<f64> = Vec::new();
    let mut lag_epochs: Vec<f64> = Vec::new();
    let mut pending = 0u64; // in window but not yet finalized in decoded data
    for (&epoch, &pts) in pivot_ts.range(..) {
        if pts < window_lo_ts || pts > max_ts {
            continue;
        }
        if epoch > max_finalized {
            pending += 1;
            continue;
        }
        let i = steps_fin.partition_point(|&f| f < epoch); // first f >= epoch
        let f_ts = steps_ts[i];
        let f_ep = steps_ep[i];
        lag_time.push(f_ts.saturating_sub(pts) as f64);
        lag_epochs.push(f_ep.saturating_sub(epoch) as f64);
    }
    anyhow::ensure!(!lag_time.is_empty(), "no finalized in-window epochs");

    lag_time.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap());
    lag_epochs.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap());

    let n = lag_time.len();
    println!("=== finalization latency (from cfxpack) | last {days}d ===");
    println!(
        "window_ts[{window_lo_ts}..{max_ts}]  finalized_epochs={n}  pending(unfinalized)={pending}",
    );
    report("lag_time  (seconds)", &lag_time);
    report("lag_epochs (epochs) ", &lag_epochs);
    Ok(())
}

/// `epoch_final_latency <packed_dir> monthly [n_months] [pos_h]`
/// Self-contained: computes the last `n_months` complete calendar months in
/// UTC+8, buckets pivot blocks by their timestamp, and emits one pivot-only
/// latency row per month. No external window specs, no blockchain_db.
fn run_monthly() -> anyhow::Result<()> {
    let packed_dir = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("usage: epoch_final_latency <packed_dir> monthly [n_months] [pos_h]"))?;
    let n_months: u64 = std::env::args().nth(3).map(|s| s.parse()).transpose()?.unwrap_or(12);
    let pos_h: u64 =
        std::env::args().nth(4).map(|s| s.parse()).transpose()?.unwrap_or(DEFAULT_POS_H);

    let mut files = collect_files(Path::new(&packed_dir))?;
    files.sort_by_key(|(_, start, _)| *start);
    // Container i spans [start_i, start_{i+1}-1]; last one is open-ended.
    let ends: Vec<u64> =
        (0..files.len()).map(|i| files.get(i + 1).map(|f| f.1 - 1).unwrap_or(u64::MAX)).collect();
    let max_start = files.last().map(|f| f.1).unwrap_or(0);
    // Decode enough tail to cover n_months complete months plus finalizer/partial slack.
    let decode_lo = max_start.saturating_sub((n_months + 2) * EPOCHS_PER_MONTH_EST);

    // Pivot-only (epoch, ts, fin_abs), sorted ascending by timestamp.
    let mut pivots: Vec<(u64, u64, u64)> = Vec::new();
    for (i, (path, start, is_zst)) in files.iter().enumerate() {
        if ends[i] < decode_lo {
            continue;
        }
        let raw = read_container(path, *is_zst)?;
        let mut dir = container::parse_directory(&raw)?;
        dir.sort_by_key(|(s, _, _, _)| *s);
        let groups: Vec<Vec<(u64, u64, u64)>> = dir
            .par_iter()
            .map(|(gstart, count, offset, length)| {
                let gend = gstart + count - 1;
                if gend < decode_lo {
                    return Vec::new();
                }
                let payload = &raw[*offset..*offset + *length];
                let packet = match decode::decode_packet_ext(payload, pos_h) {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("decode error group {gstart}-{gend}: {e:#}");
                        return Vec::new();
                    }
                };
                packet
                    .blocks
                    .iter()
                    .filter(|b| b.flags & FLAG_PIVOT != 0)
                    .map(|b| (b.epoch, b.timestamp, b.epoch.saturating_sub(b.finalized_epoch)))
                    .collect::<Vec<_>>()
            })
            .collect();
        for mut g in groups {
            pivots.append(&mut g);
        }
        eprintln!("decoded container start={start} (pivots so far {})", pivots.len());
    }
    anyhow::ensure!(!pivots.is_empty(), "no pivots decoded");
    pivots.sort_unstable_by_key(|&(_e, ts, _f)| ts);
    let data_max_ts = pivots.last().unwrap().1;

    // Global staircase of finalization progress (pivot-only), by timestamp.
    let (mut steps_fin, mut steps_ts, mut steps_ep) = (Vec::new(), Vec::new(), Vec::new());
    let mut run_max = 0u64;
    for &(epoch, ts, fin_abs) in &pivots {
        if fin_abs >= epoch {
            continue; // offset==0 => no pos_reference resolved
        }
        if fin_abs > run_max {
            run_max = fin_abs;
            steps_fin.push(run_max);
            steps_ts.push(ts);
            steps_ep.push(epoch);
        }
    }
    let max_finalized = run_max;

    let windows = monthly_windows(data_max_ts, n_months);
    eprintln!(
        "data_max_ts={data_max_ts} -> UTC+8 months [{}..{}] (trailing '*' = current incomplete month)",
        windows.first().map(|w| w.0.as_str()).unwrap_or("-"),
        windows.last().map(|w| w.0.as_str()).unwrap_or("-"),
    );
    println!("month     n_epochs  pend  |  time_s: mean p50 p90 p99 max  |  epochs: mean p50 p90 p99 max");
    for (label, ts_lo, ts_hi) in &windows {
        let mut lag_time: Vec<f64> = Vec::new();
        let mut lag_epochs: Vec<f64> = Vec::new();
        let mut pending = 0u64;
        let lo = pivots.partition_point(|&(_e, ts, _f)| ts < *ts_lo);
        for &(epoch, pts, _f) in &pivots[lo..] {
            if pts >= *ts_hi {
                break;
            }
            if epoch > max_finalized {
                pending += 1;
                continue;
            }
            let i = steps_fin.partition_point(|&f| f < epoch);
            lag_time.push(steps_ts[i].saturating_sub(pts) as f64);
            lag_epochs.push(steps_ep[i].saturating_sub(epoch) as f64);
        }
        if lag_time.is_empty() {
            println!("{label}  (no in-window finalized epochs)");
            continue;
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
    Ok(())
}

/// The last `n` complete calendar months (UTC+8) plus the current, still
/// incomplete month (labelled with a trailing `*`). Returns
/// (label "YYYY-MM"[*], ts_lo, ts_hi) oldest-first; window is
/// [month 1st 00:00 +08:00, next month 1st 00:00 +08:00) -- the partial month's
/// upper bound is in the future, so its members are simply all data up to now.
fn monthly_windows(max_ts: u64, n: u64) -> Vec<(String, u64, u64)> {
    let cur_days = (max_ts as i64 + 8 * 3600).div_euclid(86400);
    let (y, m, _d) = civil_from_days(cur_days); // current (partial) month in UTC+8
    let mut out = Vec::new();
    for k in 1..=n as i64 {
        let (yy, mm) = add_months(y, m, -k);
        let (ny, nm) = add_months(yy, mm, 1);
        out.push((format!("{yy:04}-{mm:02}"), month_start_ts(yy, mm), month_start_ts(ny, nm)));
    }
    out.reverse();
    // Current (incomplete) month, marked with '*'.
    let (ny, nm) = add_months(y, m, 1);
    out.push((format!("{y:04}-{m:02}*"), month_start_ts(y, m), month_start_ts(ny, nm)));
    out
}

/// Unix timestamp of `y-m-01 00:00:00 +08:00`.
fn month_start_ts(y: i64, m: i64) -> u64 {
    (days_from_civil(y, m, 1) * 86400 - 8 * 3600) as u64
}

fn add_months(y: i64, m: i64, delta: i64) -> (i64, i64) {
    let idx = y * 12 + (m - 1) + delta;
    (idx.div_euclid(12), idx.rem_euclid(12) + 1)
}

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Civil date (y, m, d) from days since 1970-01-01 (inverse of `days_from_civil`).
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

fn report(label: &str, sorted: &[f64]) {
    let n = sorted.len();
    let mean = sorted.iter().sum::<f64>() / n as f64;
    println!(
        "{label}: mean={:.3}  p50={:.0}  p90={:.0}  p99={:.0}  p999={:.0}  max={:.0}",
        mean,
        pctl(sorted, 0.50),
        pctl(sorted, 0.90),
        pctl(sorted, 0.99),
        pctl(sorted, 0.999),
        sorted.last().copied().unwrap_or(0.0),
    );
}

fn pctl(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = (q * (sorted.len() - 1) as f64).round() as usize;
    sorted[idx]
}

fn collect_files(dir: &Path) -> anyhow::Result<Vec<(PathBuf, u64, bool)>> {
    let mut plain: BTreeMap<u64, PathBuf> = BTreeMap::new();
    let mut zst: BTreeMap<u64, PathBuf> = BTreeMap::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        if name.ends_with(".cfxpack.zst") {
            if let Some(s) = container::start_epoch(&path) {
                zst.insert(s, path);
            }
        } else if name.ends_with(".cfxpack") {
            if let Some(s) = container::start_epoch(&path) {
                plain.insert(s, path);
            }
        }
    }
    let mut starts: Vec<u64> = plain.keys().chain(zst.keys()).copied().collect();
    starts.sort_unstable();
    starts.dedup();
    let mut out = Vec::new();
    for s in starts {
        if let Some(p) = plain.get(&s) {
            out.push((p.clone(), s, false));
        } else if let Some(p) = zst.get(&s) {
            out.push((p.clone(), s, true));
        }
    }
    anyhow::ensure!(
        !out.is_empty(),
        "no epochs_*.cfxpack[.zst] files in {} -- this tool reads the packed cfxpack \
         directory (e.g. .../packed-full), not blockchain_db",
        dir.display(),
    );
    Ok(out)
}

fn read_container(path: &Path, is_zst: bool) -> anyhow::Result<Vec<u8>> {
    if is_zst {
        use std::process::Command;
        let out = Command::new("zstd").arg("-dc").arg(path).output()?;
        anyhow::ensure!(out.status.success(), "zstd -dc failed for {}", path.display());
        Ok(out.stdout)
    } else {
        Ok(std::fs::read(path)?)
    }
}
