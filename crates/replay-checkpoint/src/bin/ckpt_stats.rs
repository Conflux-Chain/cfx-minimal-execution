// Checkpoint KV distribution inspector.
use anyhow::{Context, Result};
use cfx_minimal_mpt::MptValue;
use std::path::PathBuf;

fn main() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .expect("usage: ckpt-stats <checkpoint.bin>");
    let data = cfx_replay_checkpoint::CheckpointData::load(&path)?
        .with_context(|| format!("no checkpoint at {}", path.display()))?;

    let height = data.state.height();
    let snapshot_epoch_count = data.state.snapshot_epoch_count();

    // Snapshot: stream over the trie in place — no second BTreeMap.
    let mut snap_count = 0usize;
    let mut snap_key_bytes = 0usize;
    let mut snap_val_bytes = 0usize;
    let mut key_lens = std::collections::BTreeMap::<usize, usize>::new();
    let mut val_buckets = [0usize; 8];
    data.state.for_each_snapshot_entry(|key, value| {
        snap_count += 1;
        snap_key_bytes += key.len();
        snap_val_bytes += value.len();
        *key_lens.entry(key.len()).or_default() += 1;
        let i = match value.len() {
            0..=31 => 0,
            32..=63 => 1,
            64..=127 => 2,
            128..=255 => 3,
            256..=511 => 4,
            512..=1023 => 5,
            1024..=4095 => 6,
            _ => 7,
        };
        val_buckets[i] += 1;
        Ok(())
    })?;

    // Intermediate & delta: small BTreeMaps, borrow in place.
    let inter = data.state.intermediate_entries();
    let delta = data.state.delta_entries();

    let inter_count = inter.len();
    let inter_key_bytes: usize = inter.keys().map(|k| k.len()).sum();
    let inter_val_bytes: usize = inter.values().map(|v| match v {
        MptValue::Some(b) => b.len(),
        MptValue::Tombstone => 0,
    }).sum();
    let inter_tombstones = inter.values().filter(|v| matches!(v, MptValue::Tombstone)).count();

    let delta_count = delta.len();
    let delta_key_bytes: usize = delta.keys().map(|k| k.len()).sum();
    let delta_val_bytes: usize = delta.values().map(|v| match v {
        MptValue::Some(b) => b.len(),
        MptValue::Tombstone => 0,
    }).sum();
    let delta_tombstones = delta.values().filter(|v| matches!(v, MptValue::Tombstone)).count();

    println!("height: {}", height);
    println!("snapshot_epoch_count: {}", snapshot_epoch_count);
    println!();
    println!("=== KV counts ===");
    println!("snapshot:     {:>10} entries  keys={:.1}MB  vals={:.1}MB",
        snap_count, snap_key_bytes as f64 / 1e6, snap_val_bytes as f64 / 1e6);
    println!("intermediate: {:>10} entries  keys={:.1}MB  vals={:.1}MB  tombstones={}",
        inter_count, inter_key_bytes as f64 / 1e6, inter_val_bytes as f64 / 1e6, inter_tombstones);
    println!("delta:        {:>10} entries  keys={:.1}MB  vals={:.1}MB  tombstones={}",
        delta_count, delta_key_bytes as f64 / 1e6, delta_val_bytes as f64 / 1e6, delta_tombstones);
    println!();
    println!("total:        {:>10} entries", snap_count + inter_count + delta_count);
    println!();

    println!("=== snapshot key length distribution ===");
    for (len, count) in &key_lens {
        println!("  len={:<4} count={}", len, count);
    }

    let bucket_labels = ["0-31", "32-63", "64-127", "128-255", "256-511", "512-1023", "1024-4095", "4096+"];
    println!();
    println!("=== snapshot value size distribution ===");
    for (label, count) in bucket_labels.iter().zip(val_buckets.iter()) {
        if *count > 0 {
            println!("  {:<12} count={}", label, count);
        }
    }

    Ok(())
}
