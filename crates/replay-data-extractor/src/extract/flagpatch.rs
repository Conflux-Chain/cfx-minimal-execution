//! In-place patcher for `FLAG_ZERO_TOTAL_REWARD`.
//!
//! Setting the flag is a pure function of each block's on-chain `total_reward`,
//! which is not stored in the packet (only `base_reward` is). Rather than
//! regenerate the whole archive, this pass scans every already-written
//! `.cfxpack` container, reads each block's `total_reward` from the source DB,
//! and, for the rare blocks whose `total_reward` is zero, flips bit 6 of the
//! block's `flags` byte directly in the encoded bytes. The flag lives at a
//! fixed position inside the fixed-size record prefix, so setting it never
//! changes any length or offset — the patched file is byte-for-byte identical
//! to what a from-scratch extraction with the new logic would produce.
//!
//! This is a read-heavy / write-light task: the bottleneck is the one RocksDB
//! reward lookup per block. So we DO NOT shard by file (which would pin a whole
//! file to a single thread and process the chain out of order); instead we walk
//! files in **ascending epoch order** and fan every file's per-block reward
//! reads across all `jobs` worker threads. Progress therefore advances
//! monotonically by height, and each file gets the full read throughput.

use super::db::{open_databases, read_reward, PowDb};
use super::ExtractConfig;
use anyhow::{ensure, Context, Result};
use cfx_types::H256;
use cfxpack::decode::block_flag_sites;
use cfxpack::packet::FLAG_ZERO_TOTAL_REWARD;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

const MAGIC: &[u8; 8] = b"CFXPACK1";
const CONTAINER_HEADER_LEN: usize = 24;
const DIR_ENTRY_LEN: usize = 32;

#[derive(Default, Debug)]
pub struct FlagPatchSummary {
    pub files_scanned: usize,
    pub files_changed: usize,
    pub blocks_total: u64,
    pub blocks_flagged: u64,
    pub bytes_rewritten: u64,
    /// Paths of the files that were modified (for the consistency spot-check).
    pub changed_files: Vec<PathBuf>,
}

/// Numeric start epoch parsed from an `epochs_<start>_<end>.cfxpack` filename.
/// Used to order files by height (lexicographic order scatters the chain).
fn start_epoch(path: &Path) -> u64 {
    path.file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_prefix("epochs_"))
        .and_then(|n| n.split('_').next())
        .and_then(|n| n.parse::<u64>().ok())
        .unwrap_or(u64::MAX)
}

/// Parse a `.cfxpack` container directory into `(payload_offset, payload_len)`.
fn parse_container(data: &[u8]) -> Result<Vec<(usize, usize)>> {
    ensure!(
        data.len() >= CONTAINER_HEADER_LEN && &data[0..8] == MAGIC,
        "not a cfxpack container"
    );
    let group_count = u32::from_le_bytes(data[12..16].try_into().unwrap()) as usize;
    let mut entries = Vec::with_capacity(group_count);
    let mut pos = CONTAINER_HEADER_LEN;
    for _ in 0..group_count {
        ensure!(pos + DIR_ENTRY_LEN <= data.len(), "truncated directory");
        let offset = u64::from_le_bytes(data[pos + 16..pos + 24].try_into().unwrap()) as usize;
        let length = u64::from_le_bytes(data[pos + 24..pos + 32].try_into().unwrap()) as usize;
        ensure!(offset + length <= data.len(), "payload out of bounds");
        entries.push((offset, length));
        pos += DIR_ENTRY_LEN;
    }
    Ok(entries)
}

/// Patch one container's bytes in memory, fanning the per-block reward reads
/// across `jobs` threads. Returns `(blocks_seen, blocks_flagged, changed)`.
fn patch_container_bytes(
    data: &mut [u8],
    pow: &PowDb,
    jobs: usize,
    progress: &AtomicU64,
) -> Result<(u64, u64, bool)> {
    let entries = parse_container(data)?;

    // 1. Collect every block's (absolute flags offset, hash). `sites` is fully
    //    owned (offsets + owned hashes), so the immutable borrow of `data` ends
    //    here and we are free to mutate the flag bytes afterwards.
    let mut sites: Vec<(usize, H256)> = Vec::new();
    for (offset, length) in entries {
        for (rel_flags_offset, hash) in block_flag_sites(&data[offset..offset + length])? {
            sites.push((offset + rel_flags_offset, hash));
        }
    }
    let blocks_seen = sites.len() as u64;

    // 2. Parallel reward reads (the bottleneck). Each thread claims block
    //    indices via an atomic cursor and collects the offsets that need the
    //    flag set; the byte writes happen serially afterwards.
    let cursor = AtomicUsize::new(0);
    let flips: Mutex<Vec<usize>> = Mutex::new(Vec::new());
    let err: Mutex<Option<anyhow::Error>> = Mutex::new(None);
    let sites_ref = &sites;
    thread::scope(|scope| {
        for _ in 0..jobs.max(1) {
            let cursor = &cursor;
            let flips = &flips;
            let err = &err;
            scope.spawn(move || {
                let mut local: Vec<usize> = Vec::new();
                let mut local_progress = 0u64;
                loop {
                    let i = cursor.fetch_add(1, Ordering::Relaxed);
                    if i >= sites_ref.len() {
                        break;
                    }
                    let (abs, hash) = &sites_ref[i];
                    match read_reward(pow, hash) {
                        Ok(reward) => {
                            if reward.unwrap_or_default().total_reward.is_zero() {
                                local.push(*abs);
                            }
                        }
                        Err(e) => {
                            *err.lock().unwrap() = Some(e);
                            break;
                        }
                    }
                    local_progress += 1;
                    if local_progress >= 4096 {
                        progress.fetch_add(local_progress, Ordering::Relaxed);
                        local_progress = 0;
                    }
                }
                progress.fetch_add(local_progress, Ordering::Relaxed);
                flips.lock().unwrap().extend(local);
            });
        }
    });
    if let Some(e) = err.into_inner().unwrap() {
        return Err(e);
    }

    // 3. Apply the flag to every zero-total-reward block, serially.
    let flips = flips.into_inner().unwrap();
    let blocks_flagged = flips.len() as u64;
    let mut changed = false;
    for abs in flips {
        if data[abs] & FLAG_ZERO_TOTAL_REWARD == 0 {
            data[abs] |= FLAG_ZERO_TOTAL_REWARD;
            changed = true;
        }
    }
    Ok((blocks_seen, blocks_flagged, changed))
}

/// Scan `.cfxpack` files in ascending epoch order, set `FLAG_ZERO_TOTAL_REWARD`
/// on blocks whose `total_reward` is zero, and rewrite only the files that
/// changed. `max_epoch` (if set) limits the run to files starting below it.
pub fn add_total_reward_flag(
    config: &ExtractConfig,
    input_dir: &Path,
    jobs: usize,
    dry_run: bool,
    max_epoch: Option<u64>,
    min_epoch: Option<u64>,
) -> Result<FlagPatchSummary> {
    let (pow, _pos) = open_databases(config)?;

    let files = collect_sorted_pack_files(input_dir, max_epoch, min_epoch)?;
    let total_files = files.len();
    eprintln!(
        "flag-patch start files={} jobs={} epoch_range=[{}, {}){}",
        total_files,
        start_epoch(&files[0]),
        max_epoch
            .map(|m| m.to_string())
            .unwrap_or_else(|| "end".into()),
        if dry_run { " (dry-run)" } else { "" },
        "",
    );

    let mut summary = FlagPatchSummary::default();
    let done = AtomicU64::new(0);
    let progress = AtomicU64::new(0);
    let finished = AtomicBool::new(false);
    let started = Instant::now();

    thread::scope(|scope| -> Result<()> {
        // Dedicated monitor thread: reports real per-block throughput every 30s,
        // independent of when whole files finish.
        {
            let progress = &progress;
            let done = &done;
            let finished = &finished;
            scope.spawn(move || {
                let mut last_blocks = 0u64;
                let mut last = Instant::now();
                while !finished.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_secs(2));
                    if last.elapsed().as_secs() < 30 {
                        continue;
                    }
                    let now_blocks = progress.load(Ordering::Relaxed);
                    let dt = last.elapsed().as_secs_f64();
                    let bps = (now_blocks - last_blocks) as f64 / dt;
                    eprintln!(
                        "flag-patch progress files={}/{} blocks={} blocks/s={:.0} epochs/s~={:.0} t={}s",
                        done.load(Ordering::Relaxed),
                        total_files,
                        now_blocks,
                        bps,
                        bps / 2.72,
                        started.elapsed().as_secs(),
                    );
                    last_blocks = now_blocks;
                    last = Instant::now();
                }
            });
        }

        // Files are processed one at a time in ascending epoch order; the
        // parallelism is inside patch_container_bytes (per-block reward reads).
        for path in &files {
            let mut data = fs::read(path).with_context(|| format!("read {}", path.display()))?;
            let (seen, flagged, changed) = patch_container_bytes(&mut data, &pow, jobs, &progress)
                .with_context(|| format!("patch {}", path.display()))?;
            if changed && !dry_run {
                write_patched_file(path, &data)?;
            }
            summary.files_scanned += 1;
            summary.blocks_total += seen;
            summary.blocks_flagged += flagged;
            if changed {
                summary.files_changed += 1;
                summary.changed_files.push(path.clone());
                if !dry_run {
                    summary.bytes_rewritten += data.len() as u64;
                }
            }
            let n = done.fetch_add(1, Ordering::Relaxed) + 1;
            eprintln!(
                "flag-patch file done [{}/{}] {} start_epoch={} blocks={} flagged={} changed={} t={}s",
                n,
                total_files,
                path.file_name().and_then(|s| s.to_str()).unwrap_or("?"),
                start_epoch(path),
                seen,
                flagged,
                changed,
                started.elapsed().as_secs(),
            );
        }
        finished.store(true, Ordering::Relaxed);
        Ok(())
    })?;

    summary.changed_files.sort();
    Ok(summary)
}

/// The `.cfxpack` files under `input_dir`, in ascending on-chain epoch order
/// (NOT lexicographic filename order, which scatters the chain), optionally
/// limited to `[min_epoch, max_epoch)` so a run can resume to the chain tip.
fn collect_sorted_pack_files(
    input_dir: &Path,
    max_epoch: Option<u64>,
    min_epoch: Option<u64>,
) -> Result<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = fs::read_dir(input_dir)
        .with_context(|| format!("read dir {}", input_dir.display()))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| {
            path.extension()
                .map(|ext| ext == "cfxpack")
                .unwrap_or(false)
        })
        .collect();
    files.sort_by_key(|p| start_epoch(p));
    if let Some(limit) = max_epoch {
        files.retain(|p| start_epoch(p) < limit);
    }
    if let Some(floor) = min_epoch {
        files.retain(|p| start_epoch(p) >= floor);
    }
    ensure!(
        !files.is_empty(),
        "no .cfxpack files in {}",
        input_dir.display()
    );
    Ok(files)
}

/// Atomically replace `path` with `data` (write to a sibling `.tmp`, fsync,
/// rename) so a crash mid-write never leaves a torn container behind.
fn write_patched_file(path: &Path, data: &[u8]) -> Result<()> {
    let mut tmp = path.to_path_buf().into_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    {
        let mut f = fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path).with_context(|| format!("rename into {}", path.display()))?;
    Ok(())
}
