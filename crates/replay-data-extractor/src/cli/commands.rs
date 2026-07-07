//! One handler per CLI subcommand. [`super::run`] parses args and dispatches
//! here; each handler runs the operation and prints its summary line.

use crate::extract::{extract_packed_to_dir, ExtractConfig};
use anyhow::{Context, Result};
use cfxpack::{decode::decode_packet, packet::encode_packet, verify::verify_packet};
use std::path::PathBuf;

/// Create `path`'s parent directory if it is missing.
fn ensure_parent_dir(path: &std::path::Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    Ok(())
}

pub(super) fn cmd_verify(input: PathBuf) -> Result<()> {
    let data = std::fs::read(&input).with_context(|| format!("read packet {}", input.display()))?;
    let report = verify_packet(&data)?;
    println!(
        "verified {} bytes, first_block={}, blocks={}, tx_blocks={}, tx_items={}",
        report.packet_bytes,
        report.first_block_number,
        report.block_count,
        report.transaction_blocks,
        report.transaction_items
    );
    Ok(())
}

pub(super) fn cmd_roundtrip(input: PathBuf, reencoded_output: Option<PathBuf>) -> Result<()> {
    let data = std::fs::read(&input).with_context(|| format!("read packet {}", input.display()))?;
    let decoded = decode_packet(&data)?;
    let encoded = encode_packet(&decoded)?;
    if let Some(output) = reencoded_output {
        ensure_parent_dir(&output)?;
        std::fs::write(&output, &encoded)
            .with_context(|| format!("write packet {}", output.display()))?;
    }
    if encoded != data {
        let first_diff = data
            .iter()
            .zip(encoded.iter())
            .position(|(left, right)| left != right)
            .unwrap_or_else(|| data.len().min(encoded.len()));
        anyhow::bail!(
            "packet decode->encode is not byte-stable: original_len={}, reencoded_len={}, first_diff={}",
            data.len(),
            encoded.len(),
            first_diff
        );
    }
    let report = verify_packet(&encoded)?;
    println!(
        "roundtrip ok, bytes={}, blocks={}, tx_blocks={}, tx_items={}",
        report.packet_bytes,
        report.block_count,
        report.transaction_blocks,
        report.transaction_items
    );
    Ok(())
}

pub(super) fn cmd_extract_packed(
    config: ExtractConfig,
    output_dir: PathBuf,
    prefix: String,
    shard_epochs: u64,
    target_bytes: u64,
    jobs: usize,
) -> Result<()> {
    let summary = extract_packed_to_dir(
        &config,
        &output_dir,
        shard_epochs,
        jobs,
        target_bytes,
        &prefix,
    )?;
    println!(
        "packed ok, files={}, groups={}, epochs={}, blocks={}, tx_items={}, bytes={}, target_bytes={}, output_dir={}",
        summary.file_count,
        summary.group_count,
        summary.epoch_count,
        summary.block_count,
        summary.transaction_count,
        summary.total_bytes,
        target_bytes,
        output_dir.display()
    );
    Ok(())
}

