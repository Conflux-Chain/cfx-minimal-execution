mod commands;

use crate::extract::{ChainParams, ExtractConfig, DEFAULT_PACK_TARGET_BYTES};
use anyhow::Result;
use clap::{Parser, Subcommand};
use commands::*;
use std::path::PathBuf;

/// Resolve `--config`: a TOML file path, or the literal `mainnet` for the
/// built-in `mainnet.toml` (embedded at compile time). Omitted means mainnet.
fn load_chain(config: Option<PathBuf>) -> Result<ChainParams> {
    match config {
        Some(path) if path.as_os_str() != "mainnet" => ChainParams::from_toml_file(path),
        _ => Ok(ChainParams::mainnet()),
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
        /// Chain parameters: a TOML file path, or `mainnet` for the built-in
        /// `mainnet.toml`. Defaults to mainnet when omitted.
        #[arg(long)]
        config: Option<PathBuf>,
    },
}

pub fn run() -> Result<()> {
    match Cli::parse().command {
        Command::Verify { input } => cmd_verify(input),
        Command::Roundtrip {
            input,
            reencoded_output,
        } => cmd_roundtrip(input, reencoded_output),
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
    }
}
