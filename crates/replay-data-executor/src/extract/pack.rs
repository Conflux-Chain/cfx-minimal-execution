use super::db::{PosDb, PowDb};
use super::extract_packet_with_report;
use super::ExtractConfig;
use anyhow::{Context, Result};
use crossbeam_channel::{unbounded, Receiver, Sender};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

/// Default container target: ~100 MiB of packet payload per `.cfxpack` file.
pub const DEFAULT_PACK_TARGET_BYTES: u64 = 100 * 1024 * 1024;

const MAGIC: &[u8; 8] = b"CFXPACK1";
const FORMAT_VERSION: u32 = 1;
/// magic(8) + version(4) + group_count(4) + shard_epochs(4) + reserved(4)
const HEADER_LEN: u64 = 24;
/// start_epoch(8) + epoch_count(8) + offset(8) + length(8)
const DIR_ENTRY_LEN: u64 = 32;

#[derive(Debug, Default)]
pub struct PackSummary {
    pub file_count: usize,
    pub group_count: usize,
    pub epoch_count: u64,
    pub block_count: usize,
    pub transaction_count: usize,
    pub total_bytes: u64,
    pub files: Vec<PathBuf>,
}

pub(super) fn run_packed(
    config: &ExtractConfig,
    output_dir: &Path,
    shard_epochs: u64,
    jobs: usize,
    target_bytes: u64,
    prefix: &str,
    pow: &PowDb,
    pos: &PosDb,
) -> Result<PackSummary> {
    let tasks = build_pack_tasks(config, shard_epochs);
    let group_count = tasks.len();
    let worker_count = jobs.min(group_count);

    let (task_tx, task_rx) = unbounded::<PackTask>();
    let (result_tx, result_rx) = unbounded::<PacketOut>();
    for task in tasks {
        task_tx.send(task).expect("pack task channel open");
    }
    drop(task_tx);

    thread::scope(|scope| {
        let writer = scope.spawn(|| {
            write_containers(
                result_rx,
                output_dir,
                prefix,
                shard_epochs,
                target_bytes,
                group_count,
            )
        });

        let mut handles = Vec::with_capacity(worker_count);
        for _ in 0..worker_count {
            let task_rx = task_rx.clone();
            let result_tx = result_tx.clone();
            let base_config = config.clone();
            handles.push(scope.spawn(move || run_worker(&base_config, pow, pos, task_rx, result_tx)));
        }
        drop(result_tx);

        let mut first_error = None;
        for handle in handles {
            if let Err(error) = handle.join().expect("pack worker panicked") {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        let summary = writer.join().expect("pack writer panicked");

        // A worker failure is reported in preference to writer state, since the
        // writer may simply have stalled on the missing group.
        if let Some(error) = first_error {
            return Err(error);
        }
        summary
    })
}

fn build_pack_tasks(config: &ExtractConfig, shard_epochs: u64) -> Vec<PackTask> {
    let group_count = config.epoch_count.div_ceil(shard_epochs);
    (0..group_count)
        .map(|group_index| {
            let start_epoch = config.start_epoch + group_index * shard_epochs;
            let remaining = config.epoch_count - group_index * shard_epochs;
            let epoch_count = remaining.min(shard_epochs);
            PackTask {
                group_index,
                start_epoch,
                epoch_count,
            }
        })
        .collect()
}

fn run_worker(
    base_config: &ExtractConfig,
    pow: &PowDb,
    pos: &PosDb,
    task_rx: Receiver<PackTask>,
    result_tx: Sender<PacketOut>,
) -> Result<()> {
    for task in task_rx {
        let mut shard_config = base_config.clone();
        shard_config.start_epoch = task.start_epoch;
        shard_config.epoch_count = task.epoch_count;

        let (packet, report) = extract_packet_with_report(&shard_config, pow, pos).with_context(
            || {
                format!(
                    "extract group {}..={}",
                    task.start_epoch,
                    task.start_epoch + task.epoch_count - 1
                )
            },
        )?;

        let out = PacketOut {
            group_index: task.group_index,
            start_epoch: task.start_epoch,
            epoch_count: task.epoch_count,
            block_count: report.block_count,
            transaction_count: report.transaction_count,
            bytes: packet,
        };
        // The writer outlives all workers; a send only fails if it has already
        // returned on an IO error, in which case stopping here is correct.
        if result_tx.send(out).is_err() {
            break;
        }
    }
    Ok(())
}

/// Consume packets, emit them in group order, and flush a container file each
/// time the accumulated payload reaches `target_bytes`.
fn write_containers(
    result_rx: Receiver<PacketOut>,
    output_dir: &Path,
    prefix: &str,
    shard_epochs: u64,
    target_bytes: u64,
    group_count: usize,
) -> Result<PackSummary> {
    let mut summary = PackSummary::default();
    let mut pending: BTreeMap<u64, PacketOut> = BTreeMap::new();
    let mut next_group: u64 = 0;
    let mut builder = ContainerBuilder::default();
    let mut last_log = Instant::now();

    for packet in result_rx {
        pending.insert(packet.group_index, packet);
        while let Some(packet) = pending.remove(&next_group) {
            builder.push(packet);
            next_group += 1;
            if builder.payload_bytes >= target_bytes {
                flush_container(&mut builder, output_dir, prefix, shard_epochs, &mut summary)?;
            }
        }
        if last_log.elapsed() >= Duration::from_secs(5) {
            eprintln!(
                "extract-packed progress groups={}/{} files={} bytes={} buffered={}",
                next_group, group_count, summary.file_count, summary.total_bytes, pending.len()
            );
            last_log = Instant::now();
        }
    }

    if !builder.is_empty() {
        flush_container(&mut builder, output_dir, prefix, shard_epochs, &mut summary)?;
    }
    Ok(summary)
}

fn flush_container(
    builder: &mut ContainerBuilder,
    output_dir: &Path,
    prefix: &str,
    shard_epochs: u64,
    summary: &mut PackSummary,
) -> Result<()> {
    let start_epoch = builder.start_epoch.expect("non-empty container has a start");
    let end_epoch = builder.end_epoch;
    let path = output_dir.join(format!("{prefix}_{start_epoch}_{end_epoch}.cfxpack"));

    let group_count = builder.entries.len() as u32;
    let dir_bytes = DIR_ENTRY_LEN * u64::from(group_count);
    let payload_base = HEADER_LEN + dir_bytes;

    let file = File::create(&path).with_context(|| format!("create {}", path.display()))?;
    let mut writer = BufWriter::new(file);

    writer.write_all(MAGIC)?;
    writer.write_all(&FORMAT_VERSION.to_le_bytes())?;
    writer.write_all(&group_count.to_le_bytes())?;
    writer.write_all(&(shard_epochs as u32).to_le_bytes())?;
    writer.write_all(&0u32.to_le_bytes())?;

    let mut offset = payload_base;
    for entry in &builder.entries {
        writer.write_all(&entry.start_epoch.to_le_bytes())?;
        writer.write_all(&entry.epoch_count.to_le_bytes())?;
        writer.write_all(&offset.to_le_bytes())?;
        writer.write_all(&entry.length.to_le_bytes())?;
        offset += entry.length;
    }
    for payload in &builder.payloads {
        writer.write_all(payload)?;
    }
    writer
        .flush()
        .with_context(|| format!("flush {}", path.display()))?;

    let file_bytes = payload_base + builder.payload_bytes;
    eprintln!(
        "extract-packed wrote {} groups={} epochs={}..={} bytes={}",
        path.display(),
        group_count,
        start_epoch,
        end_epoch,
        file_bytes
    );

    summary.file_count += 1;
    summary.group_count += builder.entries.len();
    summary.epoch_count += builder.epoch_count;
    summary.block_count += builder.block_count;
    summary.transaction_count += builder.transaction_count;
    summary.total_bytes += file_bytes;
    summary.files.push(path);

    *builder = ContainerBuilder::default();
    Ok(())
}

#[derive(Default)]
struct ContainerBuilder {
    start_epoch: Option<u64>,
    end_epoch: u64,
    entries: Vec<DirEntry>,
    payloads: Vec<Vec<u8>>,
    payload_bytes: u64,
    epoch_count: u64,
    block_count: usize,
    transaction_count: usize,
}

impl ContainerBuilder {
    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn push(&mut self, packet: PacketOut) {
        if self.start_epoch.is_none() {
            self.start_epoch = Some(packet.start_epoch);
        }
        self.end_epoch = packet.start_epoch + packet.epoch_count - 1;
        let length = packet.bytes.len() as u64;
        self.entries.push(DirEntry {
            start_epoch: packet.start_epoch,
            epoch_count: packet.epoch_count,
            length,
        });
        self.payload_bytes += length;
        self.epoch_count += packet.epoch_count;
        self.block_count += packet.block_count;
        self.transaction_count += packet.transaction_count;
        self.payloads.push(packet.bytes);
    }
}

struct DirEntry {
    start_epoch: u64,
    epoch_count: u64,
    length: u64,
}

#[derive(Debug)]
struct PackTask {
    group_index: u64,
    start_epoch: u64,
    epoch_count: u64,
}

struct PacketOut {
    group_index: u64,
    start_epoch: u64,
    epoch_count: u64,
    block_count: usize,
    transaction_count: usize,
    bytes: Vec<u8>,
}
