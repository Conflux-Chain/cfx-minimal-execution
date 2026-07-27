//! Locate the finalization-latency tail: blocks whose stored confirmation depth
//! (`finalized_epoch` offset = block.epoch - finalized_abs) is abnormally large,
//! clustered into contiguous epoch segments, each cross-referenced to the PoS
//! side via `pos_view` (the PoS block height the block's pos_reference points to).
//!
//! For each segment we print the epoch range, wall-clock span (pivot timestamps),
//! instantaneous epoch rate, the offset peak, and the pos_view range -- a flat
//! pos_view while the epoch climbs means PoS finality stalled; a pos_view that
//! jumps means the epoch axis (pivot chain) jumped.
//!
//! Usage: locate_final_tail <packed_dir> [days] [offset_threshold] [pos_h] [dump_tsv]
//! packed_dir is required (no default); pass it explicitly in production.
//! With dump_tsv set, every flagged block is written as
//! `hash epoch height ts offset pos_view seg` for blockchain_db header lookup.

use cfxpack::container;
use cfxpack::decode;
use cfxpack::packet::FLAG_PIVOT;
use rayon::prelude::*;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const DEFAULT_DAYS: u64 = 30;
const DEFAULT_THR: u64 = 1000;
const DEFAULT_POS_H: u64 = 37_400_000;
const DECODE_MARGIN_EPOCHS: u64 = 2_500_000;
// Start a new segment when consecutive flagged epochs are farther apart than this.
const SEG_GAP: u64 = 2000;

#[derive(Clone, Copy)]
struct B {
    epoch: u64,
    ts: u64,
    height: u64,
    offset: u64,
    pos_view: u64,
    pivot: bool,
    hash: cfx_types::H256,
}

fn main() -> anyhow::Result<()> {
    let packed_dir = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("usage: locate_final_tail <packed_dir> [days] [offset_threshold] [pos_h]"))?;
    let days: u64 = std::env::args().nth(2).map(|s| s.parse()).transpose()?.unwrap_or(DEFAULT_DAYS);
    let thr: u64 = std::env::args().nth(3).map(|s| s.parse()).transpose()?.unwrap_or(DEFAULT_THR);
    let pos_h: u64 =
        std::env::args().nth(4).map(|s| s.parse()).transpose()?.unwrap_or(DEFAULT_POS_H);
    let dump_tsv: Option<String> = std::env::args().nth(5);

    let mut files = collect_files(Path::new(&packed_dir))?;
    files.sort_by_key(|(_, start, _)| *start);
    let max_start = files.last().map(|(_, s, _)| *s).unwrap_or(0);
    let cutoff_start = max_start.saturating_sub(DECODE_MARGIN_EPOCHS);
    let tail: Vec<_> = files.into_iter().filter(|(_, s, _)| *s >= cutoff_start).collect();
    eprintln!("decoding {} tail containers (start>={cutoff_start}) thr={thr}", tail.len());

    let mut all: Vec<B> = Vec::new();
    for (path, _start, is_zst) in &tail {
        let raw = read_container(path, *is_zst)?;
        let mut dir = container::parse_directory(&raw)?;
        dir.sort_by_key(|(start, _, _, _)| *start);
        let mut per_group: Vec<Vec<B>> = dir
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
                    .map(|b| B {
                        epoch: b.epoch,
                        ts: b.timestamp,
                        height: b.height,
                        offset: b.finalized_epoch,
                        pos_view: b.pos_view.unwrap_or(0),
                        pivot: b.flags & FLAG_PIVOT != 0,
                        hash: b.hash,
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        for g in per_group.iter_mut() {
            all.append(g);
        }
    }
    anyhow::ensure!(!all.is_empty(), "no blocks");

    let max_ts = all.iter().map(|b| b.ts).max().unwrap();
    let window_lo_ts = max_ts.saturating_sub(days * 86400);

    // pivot timestamp per epoch, for wall-clock spans and rates.
    let mut pivot_ts: BTreeMap<u64, u64> = BTreeMap::new();
    for b in &all {
        if b.pivot {
            pivot_ts.insert(b.epoch, b.ts);
        }
    }

    // Flagged blocks (large confirmation depth), in-window, sorted by epoch.
    let mut hot: Vec<B> =
        all.iter().copied().filter(|b| b.offset >= thr && b.ts >= window_lo_ts).collect();
    hot.sort_by_key(|b| b.epoch);
    eprintln!("flagged blocks (offset>={thr}, in {days}d): {}", hot.len());
    if hot.is_empty() {
        return Ok(());
    }

    println!(
        "seg  epoch_lo      epoch_hi      n_blk  n_pivot  off_max  age_max_s  age_mean_s  posview_lo  posview_hi  pv_distinct"
    );
    let mut seg_start = 0usize;
    let mut seg_id = 0usize;
    let mut dump: Option<std::io::BufWriter<std::fs::File>> = match &dump_tsv {
        Some(p) => Some(std::io::BufWriter::new(std::fs::File::create(p)?)),
        None => None,
    };
    if let Some(w) = dump.as_mut() {
        use std::io::Write;
        writeln!(w, "hash\tepoch\theight\tts\toffset\tpos_view\tseg")?;
    }
    for i in 1..=hot.len() {
        let boundary = i == hot.len() || hot[i].epoch - hot[i - 1].epoch > SEG_GAP;
        if !boundary {
            continue;
        }
        let seg = &hot[seg_start..i];
        emit_segment(seg_id, seg, &pivot_ts);
        if let Some(w) = dump.as_mut() {
            use std::io::Write;
            for b in seg {
                writeln!(
                    w,
                    "{:x}\t{}\t{}\t{}\t{}\t{}\t{}",
                    b.hash, b.epoch, b.height, b.ts, b.offset, b.pos_view, seg_id
                )?;
            }
        }
        seg_id += 1;
        seg_start = i;
    }
    if let Some(mut w) = dump {
        use std::io::Write;
        w.flush()?;
    }
    eprintln!("segments={seg_id}");
    Ok(())
}

fn emit_segment(id: usize, seg: &[B], pivot_ts: &BTreeMap<u64, u64>) {
    let e_lo = seg.first().unwrap().epoch;
    let e_hi = seg.last().unwrap().epoch;
    let ts_lo = seg.iter().map(|b| b.ts).min().unwrap();
    let ts_hi = seg.iter().map(|b| b.ts).max().unwrap();
    let off_max = seg.iter().map(|b| b.offset).max().unwrap();
    let pv_lo = seg.iter().map(|b| b.pos_view).min().unwrap();
    let pv_hi = seg.iter().map(|b| b.pos_view).max().unwrap();
    let pv_distinct = {
        let mut v: Vec<u64> = seg.iter().map(|b| b.pos_view).collect();
        v.sort_unstable();
        v.dedup();
        v.len()
    };
    // Real inclusion aging per flagged block = pivot_ts(its epoch) - its own
    // timestamp (seconds it sat before landing in the ledger). This is the
    // physical-time analogue of the epoch offset.
    let mut age_max = 0i64;
    let mut age_sum = 0i64;
    let mut age_n = 0i64;
    for b in seg {
        if let Some(&pts) = pivot_ts.get(&b.epoch) {
            let age = pts as i64 - b.ts as i64;
            age_max = age_max.max(age);
            age_sum += age;
            age_n += 1;
        }
    }
    let age_mean = if age_n > 0 { age_sum as f64 / age_n as f64 } else { 0.0 };
    let n_pivot = seg.iter().filter(|b| b.pivot).count();
    // If aging is real, age_seconds ~= offset_epochs / epoch_rate(~0.86).
    let wall = ts_hi.saturating_sub(ts_lo);
    let ep_span = e_hi - e_lo;
    let rate = if wall > 0 { ep_span as f64 / wall as f64 } else { f64::INFINITY };
    let _ = (rate, ep_span);
    println!(
        "{id:>3}  {e_lo:>12}  {e_hi:>12}  {:>5}  {n_pivot:>7}  {off_max:>7}  {age_max:>9}  {age_mean:>10.0}  {pv_lo:>10}  {pv_hi:>10}  {pv_distinct:>11}",
        seg.len(),
    );

    // Timestamp distribution of the flagged blocks: are they one instant, a few
    // seconds, or spread over the whole span? Report distinct-ts count, largest
    // single-second pileup, and a 20-bucket histogram over [ts_lo, ts_hi].
    if seg.len() >= 300 {
        let mut tss: Vec<u64> = seg.iter().map(|b| b.ts).collect();
        tss.sort_unstable();
        let distinct = {
            let mut v = tss.clone();
            v.dedup();
            v.len()
        };
        // Max blocks sharing one identical timestamp.
        let (mut max_same, mut run, mut cur) = (1usize, 1usize, tss[0]);
        for &t in &tss[1..] {
            if t == cur {
                run += 1;
                max_same = max_same.max(run);
            } else {
                run = 1;
                cur = t;
            }
        }
        let span = (ts_hi - ts_lo).max(1);
        const NB: u64 = 20;
        let mut hist = [0u32; NB as usize];
        for &t in &tss {
            let b = (((t - ts_lo) * NB) / span).min(NB - 1) as usize;
            hist[b] += 1;
        }
        let hstr: Vec<String> = hist.iter().map(|c| c.to_string()).collect();
        println!(
            "     seg{id} ts-dist: n={} distinct_ts={} max_same_second={} span_s={} | hist[20]: {}",
            seg.len(), distinct, max_same, span, hstr.join(",")
        );
    }
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
