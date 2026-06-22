use anyhow::{Context, Result};
use cfxpack::decode::decode_packet;
use crossbeam_channel::unbounded;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

#[derive(Debug, Clone)]
pub struct DecodeBenchReport {
    pub jobs: usize,
    pub packet_count: usize,
    pub total_epochs: u64,
    pub total_bytes: usize,
    pub total_blocks: usize,
    pub total_transactions: usize,
    pub load_ms: u128,
    pub decode_ms: u128,
}

pub fn bench_decode_dir(input_dir: &Path, jobs: usize) -> Result<DecodeBenchReport> {
    anyhow::ensure!(jobs > 0, "jobs must be positive");

    let load_started = Instant::now();
    let packets = read_packets(input_dir)?;
    let load_ms = load_started.elapsed().as_millis();
    let packet_count = packets.len();
    let total_epochs = packets.iter().map(|packet| packet.epoch_count).sum();
    let total_bytes = packets.iter().map(|packet| packet.data.len()).sum();

    let decode_started = Instant::now();
    let (task_tx, task_rx) = unbounded();
    for packet in packets {
        task_tx
            .send(packet)
            .expect("decode bench task channel open");
    }
    drop(task_tx);

    let worker_count = jobs.min(packet_count.max(1));
    let reports = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(worker_count);
        for _ in 0..worker_count {
            let task_rx = task_rx.clone();
            handles.push(scope.spawn(move || {
                let mut blocks = 0usize;
                let mut transactions = 0usize;
                for packet in task_rx {
                    let decoded = decode_packet(&packet.data)
                        .with_context(|| format!("decode {}", packet.path.display()))?;
                    blocks += decoded.blocks.len();
                    transactions += decoded
                        .blocks
                        .iter()
                        .map(|block| block.transactions.len())
                        .sum::<usize>();
                }
                Ok::<_, anyhow::Error>((blocks, transactions))
            }));
        }

        handles
            .into_iter()
            .map(|handle| handle.join().expect("decode bench worker panicked"))
            .collect::<Result<Vec<_>>>()
    })?;
    let decode_ms = decode_started.elapsed().as_millis();

    let (total_blocks, total_transactions) = reports
        .into_iter()
        .fold((0usize, 0usize), |(block_acc, tx_acc), (blocks, txs)| {
            (block_acc + blocks, tx_acc + txs)
        });

    Ok(DecodeBenchReport {
        jobs,
        packet_count,
        total_epochs,
        total_bytes,
        total_blocks,
        total_transactions,
        load_ms,
        decode_ms,
    })
}

#[derive(Debug)]
struct BenchPacket {
    path: PathBuf,
    epoch_count: u64,
    data: Arc<[u8]>,
}

fn read_packets(input_dir: &Path) -> Result<Vec<BenchPacket>> {
    let mut paths = std::fs::read_dir(input_dir)
        .with_context(|| format!("read input dir {}", input_dir.display()))?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("read input dir entries {}", input_dir.display()))?;
    paths.retain(|path| {
        path.extension()
            .is_some_and(|extension| extension == "cfxpkt")
    });
    paths.sort_by_key(|path| {
        shard_epoch_range(path)
            .map(|range| range.0)
            .unwrap_or(u64::MAX)
    });

    paths
        .into_iter()
        .map(|path| {
            let (start_epoch, end_epoch) = shard_epoch_range(&path)
                .with_context(|| format!("parse shard epoch range {}", path.display()))?;
            let data =
                std::fs::read(&path).with_context(|| format!("read packet {}", path.display()))?;
            Ok(BenchPacket {
                path,
                epoch_count: end_epoch - start_epoch + 1,
                data: Arc::from(data),
            })
        })
        .collect()
}

fn shard_epoch_range(path: &Path) -> Option<(u64, u64)> {
    let (start, end) = path.file_stem()?.to_str()?.split_once('-')?;
    Some((start.parse().ok()?, end.parse().ok()?))
}
