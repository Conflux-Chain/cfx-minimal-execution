use crate::{
    bench::bench_decode_dir,
    decode::decode_packet,
    extract::extract_shards_to_dir,
    extract::{
        extract_packed_to_dir, extract_raw_data, extract_to_file, ExtractConfig,
        DEFAULT_PACK_TARGET_BYTES,
    },
    raw::encode_raw_data,
    replay::validate_replay_packet,
    verify::verify_packet,
};
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "cfx-replay-data-executor")]
#[command(about = "Extract and verify Conflux execution-layer replay data packets")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Extract {
        #[arg(long, default_value = "data/blockchain_data")]
        data_dir: PathBuf,
        #[arg(long)]
        start_epoch: u64,
        #[arg(long, default_value_t = 2)]
        epoch_count: u64,
        #[arg(long, default_value = "target/replay-data/sample.cfxpkt")]
        output: PathBuf,
        #[arg(long, default_value_t = 5)]
        evm_transaction_block_ratio: u64,
        #[arg(long, default_value_t = 50)]
        pos_pivot_decision_defer_epoch_count: u64,
        #[arg(long, default_value_t = u64::MAX)]
        pos_reference_enable_height: u64,
        #[arg(long, default_value_t = u64::MAX)]
        cip112_transition_height: u64,
    },
    Verify {
        #[arg(long)]
        input: PathBuf,
    },
    Roundtrip {
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        reencoded_output: Option<PathBuf>,
    },
    Replay {
        #[arg(long)]
        input: PathBuf,
    },
    ExtractRawThenEncode {
        #[arg(long, default_value = "data/blockchain_data")]
        data_dir: PathBuf,
        #[arg(long)]
        start_epoch: u64,
        #[arg(long, default_value_t = 2)]
        epoch_count: u64,
        #[arg(long, default_value = "target/replay-data/sample.cfxpkt")]
        output: PathBuf,
    },
    ExtractShards {
        #[arg(long, default_value = "data/blockchain_data")]
        data_dir: PathBuf,
        #[arg(long)]
        start_epoch: u64,
        #[arg(long)]
        epoch_count: u64,
        #[arg(long, default_value = "target/replay-data/shards")]
        output_dir: PathBuf,
        #[arg(long, default_value_t = crate::packet::PACKET_EPOCHS)]
        shard_epochs: u64,
        #[arg(long, default_value_t = 1)]
        jobs: usize,
        #[arg(long, default_value_t = 5)]
        evm_transaction_block_ratio: u64,
        #[arg(long, default_value_t = 50)]
        pos_pivot_decision_defer_epoch_count: u64,
        #[arg(long, default_value_t = u64::MAX)]
        pos_reference_enable_height: u64,
        #[arg(long, default_value_t = u64::MAX)]
        cip112_transition_height: u64,
    },
    /// Extract epochs as 2000-epoch groups (unchanged spec) packed into
    /// ~100 MiB container files named `<prefix>_<start_epoch>_<end_epoch>.cfxpack`,
    /// each indexed so any single 2000-epoch group can be located directly.
    ExtractPacked {
        #[arg(long, default_value = "data/blockchain_data")]
        data_dir: PathBuf,
        #[arg(long)]
        start_epoch: u64,
        #[arg(long)]
        epoch_count: u64,
        #[arg(long, default_value = "target/replay-data/packed")]
        output_dir: PathBuf,
        #[arg(long, default_value = "epochs")]
        prefix: String,
        #[arg(long, default_value_t = crate::packet::PACKET_EPOCHS)]
        shard_epochs: u64,
        #[arg(long, default_value_t = DEFAULT_PACK_TARGET_BYTES)]
        target_bytes: u64,
        #[arg(long, default_value_t = 1)]
        jobs: usize,
        #[arg(long, default_value_t = 5)]
        evm_transaction_block_ratio: u64,
        #[arg(long, default_value_t = 50)]
        pos_pivot_decision_defer_epoch_count: u64,
        #[arg(long, default_value_t = u64::MAX)]
        pos_reference_enable_height: u64,
        #[arg(long, default_value_t = u64::MAX)]
        cip112_transition_height: u64,
    },
    BenchDecode {
        #[arg(long)]
        input_dir: PathBuf,
        #[arg(long, default_value_t = 1)]
        jobs: usize,
    },
}

pub fn run() -> Result<()> {
    match Cli::parse().command {
        Command::Extract {
            data_dir,
            start_epoch,
            epoch_count,
            output,
            evm_transaction_block_ratio,
            pos_pivot_decision_defer_epoch_count,
            pos_reference_enable_height,
            cip112_transition_height,
        } => {
            if let Some(parent) = output.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("create {}", parent.display()))?;
            }
            let config = ExtractConfig {
                data_dir,
                start_epoch,
                epoch_count,
                evm_transaction_block_ratio,
                pos_pivot_decision_defer_epoch_count,
                pos_reference_enable_height,
                cip112_transition_height,
            };
            let report = extract_to_file(&config, &output)?;
            println!(
                "wrote {} bytes, blocks={}, tx_items={}, output={}",
                report.packet_bytes,
                report.block_count,
                report.transaction_count,
                report.output.display()
            );
        }
        Command::Verify { input } => {
            let data = std::fs::read(&input)
                .with_context(|| format!("read packet {}", input.display()))?;
            let report = verify_packet(&data)?;
            println!(
                "verified {} bytes, first_block={}, blocks={}, tx_blocks={}, tx_items={}",
                report.packet_bytes,
                report.first_block_number,
                report.block_count,
                report.transaction_blocks,
                report.transaction_items
            );
        }
        Command::Roundtrip {
            input,
            reencoded_output,
        } => {
            let data = std::fs::read(&input)
                .with_context(|| format!("read packet {}", input.display()))?;
            let decoded = decode_packet(&data)?;
            let encoded = encode_raw_data(&decoded)?;
            if let Some(output) = reencoded_output {
                if let Some(parent) = output.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("create {}", parent.display()))?;
                }
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
        }
        Command::Replay { input } => {
            let data = std::fs::read(&input)
                .with_context(|| format!("read packet {}", input.display()))?;
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
        }
        Command::ExtractRawThenEncode {
            data_dir,
            start_epoch,
            epoch_count,
            output,
        } => {
            if let Some(parent) = output.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("create {}", parent.display()))?;
            }
            let config = ExtractConfig {
                data_dir,
                start_epoch,
                epoch_count,
                ..ExtractConfig::default()
            };
            let raw = extract_raw_data(&config)?;
            let packet = encode_raw_data(&raw)?;
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
        }
        Command::ExtractShards {
            data_dir,
            start_epoch,
            epoch_count,
            output_dir,
            shard_epochs,
            jobs,
            evm_transaction_block_ratio,
            pos_pivot_decision_defer_epoch_count,
            pos_reference_enable_height,
            cip112_transition_height,
        } => {
            let config = ExtractConfig {
                data_dir,
                start_epoch,
                epoch_count,
                evm_transaction_block_ratio,
                pos_pivot_decision_defer_epoch_count,
                pos_reference_enable_height,
                cip112_transition_height,
            };
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
                epoch_count,
                total_blocks,
                total_txs,
                total_bytes,
                output_dir.display()
            );
        }
        Command::ExtractPacked {
            data_dir,
            start_epoch,
            epoch_count,
            output_dir,
            prefix,
            shard_epochs,
            target_bytes,
            jobs,
            evm_transaction_block_ratio,
            pos_pivot_decision_defer_epoch_count,
            pos_reference_enable_height,
            cip112_transition_height,
        } => {
            let config = ExtractConfig {
                data_dir,
                start_epoch,
                epoch_count,
                evm_transaction_block_ratio,
                pos_pivot_decision_defer_epoch_count,
                pos_reference_enable_height,
                cip112_transition_height,
            };
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
        }
        Command::BenchDecode { input_dir, jobs } => {
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
        }
    }
    Ok(())
}
