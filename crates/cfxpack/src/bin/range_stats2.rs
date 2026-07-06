//! Additive/debug tool: like `range_stats`, but transactions are deduplicated
//! GLOBALLY by identity (decoded `tx.hash()`). A recycled tx (packed into
//! multiple blocks/epochs) is counted EXACTLY ONCE, attributed to the range
//! containing its FIRST occurrence in epoch order.
//!
//! Approach: iterate containers in ascending start-epoch order (= ascending
//! epoch order globally). Decode each container's 2000-epoch groups in parallel
//! (rayon), collecting per-group (tx_hash, gas) lists in epoch order plus the
//! executed-block and pivot counts. Then fold groups into a single global
//! `seen` set SEQUENTIALLY in epoch order: a tx not yet seen increments its
//! group's range unique count / unique gas and is inserted; later occurrences
//! are skipped. This yields deterministic first-occurrence attribution.
//!
//! Columns: from_height to_height blocks epochs unique_txs unique_total_gas
//!          (unique_total_gas - unique_txs*21000)
//!
//! Usage: range_stats2 [checkpoint_dir] [packed_dir] [pos_h]

use cfxpack::packet::{FLAG_PIVOT, FLAG_SKIPPED_EXECUTION};
use cfxpack::{container, decode};
use cfx_types::H256;
use rayon::prelude::*;
use rustc_hash::FxHashSet;
use std::path::{Path, PathBuf};

const DEFAULT_CKPT_DIR: &str = "/cfx-minimal-execution/data/replay-checkpoint/backup";
const DEFAULT_PACKED_DIR: &str = "/cfx-minimal-execution/data/packed-full";
const DEFAULT_POS_H: u64 = 37_400_000;
const INTRINSIC_GAS: u128 = 21_000;
const PER_OCCURRENCE_TX_TOTAL: u64 = 694_961_419;

#[derive(Clone, Default)]
struct Totals {
    blocks: u64,
    epochs: u64,
    unique_txs: u64,
    unique_gas: u128,
}

/// Per-group decode result, carried in epoch order for sequential folding.
struct GroupOut {
    range_idx: usize,
    blocks: u64,
    epochs: u64,
    txs: Vec<(H256, u128)>,
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

    // Checkpoint heights -> boundaries [0, h1, .., hn].
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

    // Containers sorted ascending by start epoch (= ascending epoch order).
    let mut files = collect_uncompressed(Path::new(&packed_dir))?;
    files.sort_by_key(|(_, start, _)| *start);

    let mut used_zst: Vec<String> = Vec::new();
    let mut seen: FxHashSet<H256> = FxHashSet::default();

    for (path, file_start, is_zst) in &files {
        if *file_start > max_boundary {
            continue;
        }
        if *is_zst {
            used_zst.push(path.display().to_string());
        }
        let raw = read_container(path, *is_zst)?;
        let mut directory = container::parse_directory(&raw)?;
        // Ensure ascending epoch order within the file for deterministic
        // first-occurrence attribution.
        directory.sort_by_key(|(start, _, _, _)| *start);

        // Decode groups in parallel. rayon's `collect` into a Vec preserves the
        // input (directory = ascending epoch) order, so `group_outs` is already
        // in epoch order for the sequential fold below.
        let group_outs: Vec<GroupOut> = directory
            .par_iter()
            .filter_map(|(start, count, offset, length)| {
                let start = *start;
                let end = start + count - 1;
                if end > max_boundary {
                    return None;
                }
                let range_idx = bucket_index(&boundaries, start)?;
                let payload = &raw[*offset..*offset + *length];
                let packet = match decode::decode_packet_ext(payload, pos_h) {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("decode error group {start}-{end}: {e:#}");
                        return None;
                    }
                };
                let mut blocks = 0u64;
                let mut epochs = 0u64;
                let mut txs: Vec<(H256, u128)> = Vec::new();
                for b in &packet.blocks {
                    if b.flags & FLAG_PIVOT != 0 {
                        epochs += 1;
                    }
                    if b.flags & FLAG_SKIPPED_EXECUTION != 0 {
                        continue;
                    }
                    blocks += 1;
                    for tx in &b.transactions {
                        txs.push((tx.hash(), tx.gas().low_u128()));
                    }
                }
                Some(GroupOut { range_idx, blocks, epochs, txs })
            })
            .collect();

        // Sequentially fold into the global seen set in epoch order.
        for g in &group_outs {
            let b = &mut buckets[g.range_idx];
            b.blocks += g.blocks;
            b.epochs += g.epochs;
            for (h, gas) in &g.txs {
                if seen.insert(*h) {
                    b.unique_txs += 1;
                    b.unique_gas += *gas;
                }
            }
        }
    }

    // Emit table.
    let mut t_blocks = 0u64;
    let mut t_epochs = 0u64;
    let mut t_txs = 0u64;
    let mut t_gas = 0u128;
    println!(
        "{:>12}  {:>12}  {:>14}  {:>10}  {:>14}  {:>24}  {:>24}",
        "from_height", "to_height", "blocks", "epochs", "unique_txs",
        "unique_total_gas", "gas_minus_21000xtx"
    );
    for i in 0..n_ranges {
        let from = boundaries[i];
        let to = boundaries[i + 1];
        let b = &buckets[i];
        t_blocks += b.blocks;
        t_epochs += b.epochs;
        t_txs += b.unique_txs;
        t_gas += b.unique_gas;
        let minus = b.unique_gas as i128 - (b.unique_txs as i128) * INTRINSIC_GAS as i128;
        println!(
            "{:>12}  {:>12}  {:>14}  {:>10}  {:>14}  {:>24}  {:>24}",
            from, to, b.blocks, b.epochs, b.unique_txs, b.unique_gas, minus
        );
    }
    let t_minus = t_gas as i128 - (t_txs as i128) * INTRINSIC_GAS as i128;
    println!(
        "{:>12}  {:>12}  {:>14}  {:>10}  {:>14}  {:>24}  {:>24}",
        "TOTAL", "", t_blocks, t_epochs, t_txs, t_gas, t_minus
    );

    eprintln!(
        "\nper-occurrence tx TOTAL = {PER_OCCURRENCE_TX_TOTAL}, unique tx TOTAL = {t_txs}, \
         deduped away = {}",
        PER_OCCURRENCE_TX_TOTAL.saturating_sub(t_txs)
    );

    if !used_zst.is_empty() {
        eprintln!("\nNOTE: fell back to .zst (uncompressed missing) for:");
        for p in &used_zst {
            eprintln!("  {p}");
        }
    }
    Ok(())
}

fn bucket_index(boundaries: &[u64], start: u64) -> Option<usize> {
    for i in 0..boundaries.len() - 1 {
        if boundaries[i] < start && start <= boundaries[i + 1] {
            return Some(i);
        }
    }
    None
}

fn collect_uncompressed(dir: &Path) -> anyhow::Result<Vec<(PathBuf, u64, bool)>> {
    use std::collections::BTreeMap;
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
