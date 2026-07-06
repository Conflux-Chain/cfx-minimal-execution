//! Additive/debug tool: aggregate per-checkpoint-range totals (executed blocks,
//! epochs = pivot count, txs, and Σ tx gas-limit) from the packed-full cfxpack
//! data. Streams containers one at a time (memory-light) and parallelizes over
//! the 2000-epoch groups within each container with rayon.
//!
//! Ranges come from the replay checkpoint filenames `ckpt_<HEIGHT>.bin.zst`:
//! sorted heights h1..hn give ranges [0,h1], [h1,h2], ..., [h_{n-1},hn].
//! Each 2000-epoch group is bucketed (by its start epoch) into the single range
//! that fully contains it (heights and groups are both 2000-aligned).
//!
//! Usage: range_stats [checkpoint_dir] [packed_dir] [pos_h]

use cfxpack::packet::{FLAG_PIVOT, FLAG_SKIPPED_EXECUTION};
use cfxpack::{container, decode};
use rayon::prelude::*;
use std::path::{Path, PathBuf};

const DEFAULT_CKPT_DIR: &str = "/cfx-minimal-execution/data/replay-checkpoint/backup";
const DEFAULT_PACKED_DIR: &str = "/cfx-minimal-execution/data/packed-full";
const DEFAULT_POS_H: u64 = 37_400_000;

#[derive(Clone, Copy, Default)]
struct Totals {
    blocks: u64,
    epochs: u64,
    txs: u64,
    total_gas: u128,
}

impl Totals {
    fn add(&mut self, o: &Totals) {
        self.blocks += o.blocks;
        self.epochs += o.epochs;
        self.txs += o.txs;
        self.total_gas += o.total_gas;
    }
}

fn main() -> anyhow::Result<()> {
    let ckpt_dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| DEFAULT_CKPT_DIR.to_string());
    let packed_dir = std::env::args()
        .nth(2)
        .unwrap_or_else(|| DEFAULT_PACKED_DIR.to_string());
    let pos_h: u64 = std::env::args()
        .nth(3)
        .map(|s| s.parse())
        .transpose()?
        .unwrap_or(DEFAULT_POS_H);

    // 1. Checkpoint heights from filenames -> sorted boundaries [0, h1, .., hn].
    let mut heights: Vec<u64> = Vec::new();
    for entry in std::fs::read_dir(&ckpt_dir)? {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if let Some(rest) = name.strip_prefix("ckpt_") {
            if let Some(h) = rest.strip_suffix(".bin.zst") {
                if let Ok(v) = h.parse::<u64>() {
                    heights.push(v);
                }
            }
        }
    }
    heights.sort_unstable();
    heights.dedup();
    anyhow::ensure!(!heights.is_empty(), "no ckpt_*.bin.zst in {ckpt_dir}");

    let mut boundaries: Vec<u64> = Vec::with_capacity(heights.len() + 1);
    boundaries.push(0);
    boundaries.extend_from_slice(&heights);
    let max_boundary = *boundaries.last().unwrap();
    let n_ranges = boundaries.len() - 1;
    let mut buckets = vec![Totals::default(); n_ranges];

    eprintln!(
        "ranges: {} boundaries (0..{max_boundary}), pos_h={pos_h}",
        boundaries.len()
    );

    // 2. Uncompressed .cfxpack files, sorted by start epoch, prefer plain over .zst.
    let mut files = collect_uncompressed(Path::new(&packed_dir))?;
    files.sort_by_key(|(_, start, _)| *start);

    let mut used_zst: Vec<String> = Vec::new();

    // 3. Stream containers sequentially; parallelize groups within each.
    for (path, file_start, is_zst) in &files {
        // Skip files entirely beyond the last checkpoint boundary.
        if *file_start > max_boundary {
            continue;
        }
        if *is_zst {
            used_zst.push(path.display().to_string());
        }
        let raw = read_container(path, *is_zst)?;
        let directory = container::parse_directory(&raw)?;

        let group_results: Vec<(u64, u64, Totals)> = directory
            .par_iter()
            .filter_map(|(start, count, offset, length)| {
                let start = *start;
                let end = start + count - 1;
                // Only groups fully inside [0, max_boundary].
                if end > max_boundary {
                    return None;
                }
                let payload = &raw[*offset..*offset + *length];
                let packet = match decode::decode_packet_ext(payload, pos_h) {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("decode error group {start}-{end}: {e:#}");
                        return None;
                    }
                };
                let mut t = Totals::default();
                for b in &packet.blocks {
                    // epochs = decoded pivot count (over all blocks).
                    if b.flags & FLAG_PIVOT != 0 {
                        t.epochs += 1;
                    }
                    // executed blocks: strip SKIPPED_EXECUTION.
                    if b.flags & FLAG_SKIPPED_EXECUTION != 0 {
                        continue;
                    }
                    t.blocks += 1;
                    t.txs += b.transactions.len() as u64;
                    for tx in &b.transactions {
                        t.total_gas += u256_to_u128(tx.gas());
                    }
                }
                Some((start, end, t))
            })
            .collect();

        for (start, _end, t) in &group_results {
            let idx = bucket_index(&boundaries, *start)
                .unwrap_or_else(|| panic!("group start {start} did not bucket"));
            buckets[idx].add(t);
        }
        // `raw` dropped here before reading the next container.
    }

    // 4. Emit table.
    let mut total = Totals::default();
    println!(
        "{:>12}  {:>12}  {:>14}  {:>10}  {:>14}  {:>26}",
        "from_height", "to_height", "blocks", "epochs", "txs", "total_gas"
    );
    for i in 0..n_ranges {
        let from = boundaries[i];
        let to = boundaries[i + 1];
        let b = &buckets[i];
        total.add(b);
        println!(
            "{:>12}  {:>12}  {:>14}  {:>10}  {:>14}  {:>26}",
            from, to, b.blocks, b.epochs, b.txs, b.total_gas
        );
    }
    println!(
        "{:>12}  {:>12}  {:>14}  {:>10}  {:>14}  {:>26}",
        "TOTAL", "", total.blocks, total.epochs, total.txs, total.total_gas
    );

    if !used_zst.is_empty() {
        eprintln!("\nNOTE: fell back to .zst (uncompressed missing) for:");
        for p in &used_zst {
            eprintln!("  {p}");
        }
    }
    Ok(())
}

/// Find range index i such that boundaries[i] < start <= boundaries[i+1].
fn bucket_index(boundaries: &[u64], start: u64) -> Option<usize> {
    for i in 0..boundaries.len() - 1 {
        if boundaries[i] < start && start <= boundaries[i + 1] {
            return Some(i);
        }
    }
    None
}

fn u256_to_u128(v: &cfx_types::U256) -> u128 {
    // tx gas limits are far below u128; low_u128 is exact here.
    v.low_u128()
}

/// For each epoch range, choose the uncompressed `.cfxpack` if present, else the
/// `.cfxpack.zst`. Returns (path, start_epoch, is_zst).
fn collect_uncompressed(dir: &Path) -> anyhow::Result<Vec<(PathBuf, u64, bool)>> {
    use std::collections::BTreeMap;
    // start -> (has_plain path?, zst path?)
    let mut plain: BTreeMap<u64, PathBuf> = BTreeMap::new();
    let mut zst: BTreeMap<u64, PathBuf> = BTreeMap::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
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
    let mut out = Vec::new();
    let mut starts: Vec<u64> = plain.keys().chain(zst.keys()).copied().collect();
    starts.sort_unstable();
    starts.dedup();
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
        // Decompress via the `zstd` CLI to avoid adding a crate dependency.
        use std::process::Command;
        let out = Command::new("zstd")
            .arg("-dc")
            .arg(path)
            .output()
            .map_err(|e| anyhow::anyhow!("spawn zstd for {}: {e}", path.display()))?;
        anyhow::ensure!(
            out.status.success(),
            "zstd -dc failed for {}: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(out.stdout)
    } else {
        Ok(std::fs::read(path)?)
    }
}
