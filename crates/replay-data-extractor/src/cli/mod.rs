mod commands;

use crate::extract::{ChainParams, ExtractConfig, DEFAULT_PACK_TARGET_BYTES};
use anyhow::Result;
use clap::{Parser, Subcommand};
use commands::*;
use std::path::PathBuf;

/// Load chain parameters from a TOML config file, or fall back to test defaults
/// (PoS/CIP-112 disabled) when no `--config` is given.
fn load_chain(config: Option<PathBuf>) -> Result<ChainParams> {
    match config {
        Some(path) => ChainParams::from_toml_file(path),
        None => Ok(ChainParams::default()),
    }
}

#[derive(Debug, Parser)]
#[command(name = "cfx-replay-data-extractor")]
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
        /// TOML file with chain parameters (evm_transaction_block_ratio,
        /// pos_reference_enable_height, cip112_transition_height). Omit for
        /// test defaults (PoS/CIP-112 disabled).
        #[arg(long)]
        config: Option<PathBuf>,
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
        #[arg(long, default_value_t = cfxpack::packet::PACKET_EPOCHS)]
        shard_epochs: u64,
        #[arg(long, default_value_t = 1)]
        jobs: usize,
        /// TOML file with chain parameters; omit for test defaults.
        #[arg(long)]
        config: Option<PathBuf>,
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
        #[arg(long, default_value_t = cfxpack::packet::PACKET_EPOCHS)]
        shard_epochs: u64,
        #[arg(long, default_value_t = DEFAULT_PACK_TARGET_BYTES)]
        target_bytes: u64,
        #[arg(long, default_value_t = 1)]
        jobs: usize,
        /// TOML file with chain parameters (mainnet activation heights). Omit
        /// for test defaults (PoS/CIP-112 disabled).
        #[arg(long)]
        config: Option<PathBuf>,
    },
    BenchDecode {
        #[arg(long)]
        input_dir: PathBuf,
        #[arg(long, default_value_t = 1)]
        jobs: usize,
    },
    /// Scan an existing `.cfxpack` archive and set `FLAG_ZERO_TOTAL_REWARD`
    /// in place on every block whose on-chain `total_reward` is zero. Only the
    /// files that actually contain such a block are rewritten.
    AddTotalRewardFlag {
        #[arg(long)]
        input_dir: PathBuf,
        #[arg(long, default_value = "data/blockchain_data")]
        data_dir: PathBuf,
        #[arg(long, default_value_t = 16)]
        jobs: usize,
        /// Scan and report only; do not rewrite any file.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
        /// Only process files whose start epoch is below this height
        /// (ascending epoch order). Omit to process the whole archive.
        #[arg(long)]
        max_epoch: Option<u64>,
        /// Only process files whose start epoch is at or above this height.
        /// Use to resume a run that already covered the lower epochs.
        #[arg(long)]
        min_epoch: Option<u64>,
    },
}

pub fn run() -> Result<()> {
    match Cli::parse().command {
        Command::Extract {
            data_dir,
            start_epoch,
            epoch_count,
            output,
            config,
        } => cmd_extract(
            ExtractConfig {
                data_dir,
                start_epoch,
                epoch_count,
                chain: load_chain(config)?,
            },
            output,
        ),
        Command::Verify { input } => cmd_verify(input),
        Command::Roundtrip {
            input,
            reencoded_output,
        } => cmd_roundtrip(input, reencoded_output),
        Command::Replay { input } => cmd_replay(input),
        Command::ExtractRawThenEncode {
            data_dir,
            start_epoch,
            epoch_count,
            output,
        } => cmd_extract_raw_then_encode(
            ExtractConfig {
                data_dir,
                start_epoch,
                epoch_count,
                ..ExtractConfig::default()
            },
            output,
        ),
        Command::ExtractShards {
            data_dir,
            start_epoch,
            epoch_count,
            output_dir,
            shard_epochs,
            jobs,
            config,
        } => cmd_extract_shards(
            ExtractConfig {
                data_dir,
                start_epoch,
                epoch_count,
                chain: load_chain(config)?,
            },
            output_dir,
            shard_epochs,
            jobs,
        ),
        Command::ExtractPacked {
            data_dir,
            start_epoch,
            epoch_count,
            output_dir,
            prefix,
            shard_epochs,
            target_bytes,
            jobs,
            config,
        } => cmd_extract_packed(
            ExtractConfig {
                data_dir,
                start_epoch,
                epoch_count,
                chain: load_chain(config)?,
            },
            output_dir,
            prefix,
            shard_epochs,
            target_bytes,
            jobs,
        ),
        Command::BenchDecode { input_dir, jobs } => cmd_bench_decode(input_dir, jobs),
        Command::AddTotalRewardFlag {
            input_dir,
            data_dir,
            jobs,
            dry_run,
            max_epoch,
            min_epoch,
        } => cmd_add_total_reward_flag(
            ExtractConfig {
                data_dir,
                ..ExtractConfig::default()
            },
            input_dir,
            jobs,
            dry_run,
            max_epoch,
            min_epoch,
        ),
    }
}
