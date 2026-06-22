use super::{Databases, ExtractConfig, ExtractReport};
use anyhow::{Context, Result};
use crossbeam_channel::{unbounded, Receiver, RecvTimeoutError, Sender};
use std::{
    path::{Path, PathBuf},
    thread::{self, ScopedJoinHandle},
    time::{Duration, Instant},
};

pub(super) fn run_shards(
    config: &ExtractConfig,
    output_dir: &Path,
    shard_epochs: u64,
    jobs: usize,
    dbs: &Databases,
) -> Result<Vec<ExtractReport>> {
    let tasks = build_shard_tasks(config, output_dir, shard_epochs);
    let task_count = tasks.len();
    let worker_count = jobs.min(task_count);
    let (task_tx, task_rx) = unbounded();
    let (progress_tx, progress_rx) = unbounded();

    for task in tasks {
        task_tx.send(task).expect("extract shard task channel open");
    }
    drop(task_tx);

    thread::scope(|scope| {
        let logger =
            scope.spawn(move || log_progress_events(progress_rx, task_count, worker_count));
        let mut handles = Vec::with_capacity(worker_count);

        for worker_id in 0..worker_count {
            let task_rx = task_rx.clone();
            let progress_tx = progress_tx.clone();
            let base_config = config.clone();
            handles.push(
                scope.spawn(move || run_worker(worker_id, &base_config, dbs, task_rx, progress_tx)),
            );
        }
        drop(progress_tx);

        let reports = collect_worker_reports(handles);
        logger.join().expect("extract progress logger panicked");
        reports
    })
}

fn build_shard_tasks(
    config: &ExtractConfig,
    output_dir: &Path,
    shard_epochs: u64,
) -> Vec<ShardTask> {
    let shard_count = config.epoch_count.div_ceil(shard_epochs);
    (0..shard_count)
        .map(|shard_index| {
            let start_epoch = config.start_epoch + shard_index * shard_epochs;
            let remaining = config.epoch_count - shard_index * shard_epochs;
            let epoch_count = remaining.min(shard_epochs);
            let end_epoch = start_epoch + epoch_count - 1;
            ShardTask {
                start_epoch,
                epoch_count,
                output: output_dir.join(format!("{start_epoch}-{end_epoch}.cfxpkt")),
            }
        })
        .collect()
}

fn run_worker(
    worker_id: usize,
    base_config: &ExtractConfig,
    dbs: &Databases,
    task_rx: Receiver<ShardTask>,
    progress_tx: Sender<ProgressEvent>,
) -> Result<Vec<ExtractReport>> {
    let mut reports = Vec::new();
    for task in task_rx {
        reports.push(run_shard_task(
            worker_id,
            base_config,
            dbs,
            task,
            &progress_tx,
        )?);
    }
    Ok(reports)
}

fn run_shard_task(
    worker_id: usize,
    base_config: &ExtractConfig,
    dbs: &Databases,
    task: ShardTask,
    progress_tx: &Sender<ProgressEvent>,
) -> Result<ExtractReport> {
    let started = Instant::now();
    let active = ActiveShard {
        start_epoch: task.start_epoch,
        end_epoch: task.end_epoch(),
        started,
    };
    let _ = progress_tx.send(ProgressEvent::Start { worker_id, active });
    log_shard_start(worker_id, &task);

    let mut shard_config = base_config.clone();
    shard_config.start_epoch = task.start_epoch;
    shard_config.epoch_count = task.epoch_count;

    match dbs.to_file(&shard_config, &task.output) {
        Ok(report) => {
            log_shard_done(worker_id, &report, started);
            let _ = progress_tx.send(ProgressEvent::Done { worker_id });
            Ok(report)
        }
        Err(error) => {
            let _ = progress_tx.send(ProgressEvent::Failed { worker_id });
            eprintln!(
                "extract-shard error worker={} epochs={}..={} elapsed_ms={} error={:#}",
                worker_id,
                task.start_epoch,
                task.end_epoch(),
                started.elapsed().as_millis(),
                error
            );
            Err(error).with_context(|| {
                format!("extract shard {}..={}", task.start_epoch, task.end_epoch())
            })
        }
    }
}

fn collect_worker_reports(
    handles: Vec<ScopedJoinHandle<'_, Result<Vec<ExtractReport>>>>,
) -> Result<Vec<ExtractReport>> {
    let mut reports = Vec::new();
    let mut first_error = None;
    for handle in handles {
        match handle.join().expect("extract worker panicked") {
            Ok(worker_reports) => reports.extend(worker_reports),
            Err(error) => {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
    }
    if let Some(error) = first_error {
        Err(error)
    } else {
        Ok(reports)
    }
}

fn log_progress_events(
    progress_rx: Receiver<ProgressEvent>,
    task_count: usize,
    worker_count: usize,
) {
    let mut progress = ProgressState::new(task_count, worker_count);
    let mut last_log = Instant::now();

    loop {
        match progress_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(event) => progress.apply(event),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        if last_log.elapsed() >= Duration::from_secs(5) {
            progress.log();
            last_log = Instant::now();
        }
    }
    progress.log();
}

fn log_shard_start(worker_id: usize, task: &ShardTask) {
    eprintln!(
        "extract-shard start worker={} epochs={}..={} output={}",
        worker_id,
        task.start_epoch,
        task.end_epoch(),
        task.output.display()
    );
}

fn log_shard_done(worker_id: usize, report: &ExtractReport, started: Instant) {
    let timing = &report.timing;
    eprintln!(
        "extract-shard done worker={} epochs={}..={} elapsed_ms={} load_epochs_ms={} read_blocks_ms={} build_tables_ms={} build_blocks_ms={} encode_ms={} verify_ms={} write_ms={} bytes={} blocks={} tx_items={} output={}",
        worker_id,
        report.start_epoch,
        report.start_epoch + report.epoch_count - 1,
        started.elapsed().as_millis(),
        timing.load_epochs_ms,
        timing.read_blocks_ms,
        timing.build_tables_ms,
        timing.build_blocks_ms,
        timing.encode_ms,
        timing.verify_ms,
        timing.write_ms,
        report.packet_bytes,
        report.block_count,
        report.transaction_count,
        report.output.display()
    );
}

#[derive(Debug)]
struct ShardTask {
    start_epoch: u64,
    epoch_count: u64,
    output: PathBuf,
}

impl ShardTask {
    fn end_epoch(&self) -> u64 {
        self.start_epoch + self.epoch_count - 1
    }
}

enum ProgressEvent {
    Start {
        worker_id: usize,
        active: ActiveShard,
    },
    Done {
        worker_id: usize,
    },
    Failed {
        worker_id: usize,
    },
}

struct ProgressState {
    total: usize,
    completed: usize,
    failed: usize,
    active: Vec<Option<ActiveShard>>,
}

impl ProgressState {
    fn new(total: usize, worker_count: usize) -> Self {
        Self {
            total,
            completed: 0,
            failed: 0,
            active: vec![None; worker_count],
        }
    }

    fn apply(&mut self, event: ProgressEvent) {
        match event {
            ProgressEvent::Start { worker_id, active } => {
                self.active[worker_id] = Some(active);
            }
            ProgressEvent::Done { worker_id } => {
                self.completed += 1;
                self.active[worker_id] = None;
            }
            ProgressEvent::Failed { worker_id } => {
                self.failed += 1;
                self.active[worker_id] = None;
            }
        }
    }

    fn log(&self) {
        let active_count = self.active.iter().filter(|entry| entry.is_some()).count();
        let active = self
            .active
            .iter()
            .enumerate()
            .map(|(worker, active)| match active {
                Some(active) => format!(
                    "worker{}={}..={} running_ms={}",
                    worker,
                    active.start_epoch,
                    active.end_epoch,
                    active.started.elapsed().as_millis()
                ),
                None => format!("worker{}=idle", worker),
            })
            .collect::<Vec<_>>()
            .join(" ");
        eprintln!(
            "extract-shards progress total={} completed={} failed={} active={} {}",
            self.total, self.completed, self.failed, active_count, active
        );
    }
}

#[derive(Clone, Copy)]
struct ActiveShard {
    start_epoch: u64,
    end_epoch: u64,
    started: Instant,
}
