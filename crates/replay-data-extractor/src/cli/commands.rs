//! One handler per CLI subcommand. [`super::run`] parses args and dispatches
//! here; each handler runs the operation and prints its summary line.

use crate::{
    bench::bench_decode_dir,
    extract::{
        add_total_reward_flag, extract_packed_to_dir, extract_raw_data, extract_shards_to_dir,
        extract_to_file, ExtractConfig,
    },
    validate::validate_replay_packet,
};
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

pub(super) fn cmd_extract(config: ExtractConfig, output: PathBuf) -> Result<()> {
    ensure_parent_dir(&output)?;
    let report = extract_to_file(&config, &output)?;
    println!(
        "wrote {} bytes, blocks={}, tx_items={}, output={}",
        report.packet_bytes,
        report.block_count,
        report.transaction_count,
        report.output.display()
    );
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

pub(super) fn cmd_replay(input: PathBuf) -> Result<()> {
    let data = std::fs::read(&input).with_context(|| format!("read packet {}", input.display()))?;
    let report = validate_replay_packet(&data)?;
    println!(
        "replay plan ok, bytes={}, epochs={}, blocks={}, tx_items={}, duplicate_txs={}, native_txs={}, espace_txs={}, block_numbers={}..={}",
        report.packet_bytes,
        report.epoch_count,
        report.block_count,
        report.transaction_count,
        report.duplicate_transaction_count,
        report.native_transaction_count,
        report.espace_transaction_count,
        report.first_block_number,
        report.last_block_number
    );
    Ok(())
}

pub(super) fn cmd_extract_raw_then_encode(config: ExtractConfig, output: PathBuf) -> Result<()> {
    ensure_parent_dir(&output)?;
    let raw = extract_raw_data(&config)?;
    let packet = encode_packet(&raw)?;
    std::fs::write(&output, &packet)
        .with_context(|| format!("write packet {}", output.display()))?;
    let report = verify_packet(&packet)?;
    println!(
        "raw->packet ok, bytes={}, blocks={}, tx_items={}, output={}",
        report.packet_bytes,
        report.block_count,
        report.transaction_items,
        output.display()
    );
    Ok(())
}

pub(super) fn cmd_extract_shards(
    config: ExtractConfig,
    output_dir: PathBuf,
    shard_epochs: u64,
    jobs: usize,
) -> Result<()> {
    let reports = extract_shards_to_dir(&config, &output_dir, shard_epochs, jobs)?;
    let total_blocks: usize = reports.iter().map(|report| report.block_count).sum();
    let total_txs: usize = reports.iter().map(|report| report.transaction_count).sum();
    let total_bytes: usize = reports.iter().map(|report| report.packet_bytes).sum();
    let shard_count = reports.len() as u128;
    let total_load_epochs_ms: u128 = reports
        .iter()
        .map(|report| report.timing.load_epochs_ms)
        .sum();
    let total_read_blocks_ms: u128 = reports
        .iter()
        .map(|report| report.timing.read_blocks_ms)
        .sum();
    let total_build_tables_ms: u128 = reports
        .iter()
        .map(|report| report.timing.build_tables_ms)
        .sum();
    let total_build_blocks_ms: u128 = reports
        .iter()
        .map(|report| report.timing.build_blocks_ms)
        .sum();
    let total_encode_ms: u128 = reports.iter().map(|report| report.timing.encode_ms).sum();
    let total_verify_ms: u128 = reports.iter().map(|report| report.timing.verify_ms).sum();
    let total_write_ms: u128 = reports.iter().map(|report| report.timing.write_ms).sum();
    for report in &reports {
        println!(
            "wrote {} bytes, epochs={}..={}, blocks={}, tx_items={}, output={}",
            report.packet_bytes,
            report.start_epoch,
            report.start_epoch + report.epoch_count - 1,
            report.block_count,
            report.transaction_count,
            report.output.display()
        );
    }
    println!(
        "time break total_ms load_epochs={} read_blocks={} build_tables={} build_blocks={} encode={} verify={} write={}",
        total_load_epochs_ms,
        total_read_blocks_ms,
        total_build_tables_ms,
        total_build_blocks_ms,
        total_encode_ms,
        total_verify_ms,
        total_write_ms
    );
    println!(
        "time break avg_ms_per_shard load_epochs={} read_blocks={} build_tables={} build_blocks={} encode={} verify={} write={}",
        total_load_epochs_ms / shard_count,
        total_read_blocks_ms / shard_count,
        total_build_tables_ms / shard_count,
        total_build_blocks_ms / shard_count,
        total_encode_ms / shard_count,
        total_verify_ms / shard_count,
        total_write_ms / shard_count
    );
    println!(
        "shards ok, shard_count={}, epochs={}, blocks={}, tx_items={}, bytes={}, output_dir={}",
        reports.len(),
        config.epoch_count,
        total_blocks,
        total_txs,
        total_bytes,
        output_dir.display()
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

pub(super) fn cmd_bench_decode(input_dir: PathBuf, jobs: usize) -> Result<()> {
    let report = bench_decode_dir(&input_dir, jobs)?;
    let decode_sec = report.decode_ms as f64 / 1000.0;
    println!(
        "decode bench ok, jobs={}, packets={}, epochs={}, bytes={}, blocks={}, tx_items={}, load_ms={}, decode_ms={}, epochs_per_sec={:.2}, blocks_per_sec={:.2}",
        report.jobs,
        report.packet_count,
        report.total_epochs,
        report.total_bytes,
        report.total_blocks,
        report.total_transactions,
        report.load_ms,
        report.decode_ms,
        report.total_epochs as f64 / decode_sec,
        report.total_blocks as f64 / decode_sec
    );
    Ok(())
}

pub(super) fn cmd_add_total_reward_flag(
    config: ExtractConfig,
    input_dir: PathBuf,
    jobs: usize,
    dry_run: bool,
    max_epoch: Option<u64>,
    min_epoch: Option<u64>,
) -> Result<()> {
    let summary = add_total_reward_flag(&config, &input_dir, jobs, dry_run, max_epoch, min_epoch)?;
    println!(
        "total-reward-flag {} ok, files_scanned={}, files_changed={}, blocks_total={}, blocks_flagged={} ({:.4}%), bytes_rewritten={}",
        if dry_run { "dry-run" } else { "patch" },
        summary.files_scanned,
        summary.files_changed,
        summary.blocks_total,
        summary.blocks_flagged,
        if summary.blocks_total > 0 {
            summary.blocks_flagged as f64 / summary.blocks_total as f64 * 100.0
        } else {
            0.0
        },
        summary.bytes_rewritten,
    );
    Ok(())
}
