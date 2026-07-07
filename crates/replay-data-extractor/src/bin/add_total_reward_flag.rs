// One-time archive migration: scan an existing `.cfxpack` archive and set
// `FLAG_ZERO_TOTAL_REWARD` in place on every block whose on-chain
// `total_reward` is zero. Newly extracted archives already carry the flag
// (set during extraction), so this only matters for archives produced before
// the flag existed. Kept as a standalone bin, not a pipeline command.
use anyhow::Result;
use cfx_replay_data_extractor::extract::{add_total_reward_flag, ExtractConfig};
use clap::Parser;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "add_total_reward_flag")]
#[command(about = "Patch FLAG_ZERO_TOTAL_REWARD into a pre-existing .cfxpack archive")]
struct Cli {
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
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = ExtractConfig {
        data_dir: cli.data_dir,
        ..ExtractConfig::default()
    };
    let summary = add_total_reward_flag(
        &config,
        &cli.input_dir,
        cli.jobs,
        cli.dry_run,
        cli.max_epoch,
        cli.min_epoch,
    )?;
    println!(
        "total-reward-flag {} ok, files_scanned={}, files_changed={}, blocks_total={}, blocks_flagged={} ({:.4}%), bytes_rewritten={}",
        if cli.dry_run { "dry-run" } else { "patch" },
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
