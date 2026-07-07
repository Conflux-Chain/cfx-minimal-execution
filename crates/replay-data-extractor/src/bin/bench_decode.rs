// Decode-throughput benchmark over a directory of extracted packets. A
// standalone measurement tool, not part of the extraction pipeline — kept out
// of the main CLI on purpose.
use anyhow::Result;
use cfx_replay_data_extractor::bench::bench_decode_dir;
use clap::Parser;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "bench_decode")]
#[command(about = "Benchmark packet decode throughput over a directory")]
struct Cli {
    #[arg(long)]
    input_dir: PathBuf,
    #[arg(long, default_value_t = 1)]
    jobs: usize,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let report = bench_decode_dir(&cli.input_dir, cli.jobs)?;
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
