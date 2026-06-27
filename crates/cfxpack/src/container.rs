//! The outer `.cfxpack` container: a magic-tagged header plus a directory of
//! 2000-epoch group entries. It is the file-level wrapper around the per-group
//! packets that [`crate::packet`] encodes — the extractor's packer writes it
//! (using the constants here), and the replayer reads it back.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

pub const MAGIC: &[u8; 8] = b"CFXPACK1";
pub const FORMAT_VERSION: u32 = 1;
/// magic(8) + version(4) + group_count(4) + shard_epochs(4) + reserved(4)
pub const HEADER_LEN: u64 = 24;
/// start_epoch(8) + epoch_count(8) + offset(8) + length(8)
pub const DIR_ENTRY_LEN: u64 = 32;

/// All `.cfxpack` (or `.cfxpack.zst`) files in `dir`, sorted by start epoch.
/// When both compressed and uncompressed variants exist for the same epoch
/// range, the `.zst` file wins.
pub fn collect_files(dir: &Path) -> Result<Vec<PathBuf>> {
    use std::collections::BTreeMap;
    let mut by_start: BTreeMap<u64, PathBuf> = BTreeMap::new();
    for entry in std::fs::read_dir(dir)
        .with_context(|| format!("read dir {}", dir.display()))?
    {
        let path = entry?.path();
        if !is_cfxpack(&path) {
            continue;
        }
        let Some(start) = start_epoch(&path) else { continue };
        let prefer_new = is_compressed(&path)
            || !by_start.get(&start).is_some_and(|old| is_compressed(old));
        if prefer_new {
            by_start.insert(start, path);
        }
    }
    let files: Vec<PathBuf> = by_start.into_values().collect();
    anyhow::ensure!(!files.is_empty(), "no .cfxpack files in {}", dir.display());
    Ok(files)
}

/// Enforce that groups arrive as one contiguous, gap-free epoch sequence, and
/// that the first pending group lines up with the resume height (if resuming).
pub fn validate_contiguity(
    start_epoch: u64,
    next_expected: &mut Option<u64>,
    resume_height: u64,
) -> Result<()> {
    match *next_expected {
        Some(expected) => anyhow::ensure!(
            start_epoch == expected,
            "non-contiguous groups: expected start epoch {expected}, got {start_epoch}",
        ),
        None => anyhow::ensure!(
            resume_height == 0 || start_epoch == resume_height + 1,
            "resume gap: checkpoint height {resume_height}, first pending group starts at epoch {start_epoch}",
        ),
    }
    Ok(())
}

/// Parse the container directory, returning `(start_epoch, epoch_count,
/// payload_offset, payload_length)` per 2000-epoch group, in file order.
pub fn parse_directory(data: &[u8]) -> Result<Vec<(u64, u64, usize, usize)>> {
    let header_len = HEADER_LEN as usize;
    let entry_len = DIR_ENTRY_LEN as usize;
    anyhow::ensure!(
        data.len() >= header_len && &data[0..8] == MAGIC,
        "not a cfxpack container"
    );
    let group_count = u32::from_le_bytes(data[12..16].try_into()?) as usize;
    let mut entries = Vec::with_capacity(group_count);
    let mut pos = header_len;
    for _ in 0..group_count {
        anyhow::ensure!(pos + entry_len <= data.len(), "truncated directory");
        let start_epoch = u64::from_le_bytes(data[pos..pos + 8].try_into()?);
        let epoch_count = u64::from_le_bytes(data[pos + 8..pos + 16].try_into()?);
        let offset = u64::from_le_bytes(data[pos + 16..pos + 24].try_into()?) as usize;
        let length = u64::from_le_bytes(data[pos + 24..pos + 32].try_into()?) as usize;
        anyhow::ensure!(offset + length <= data.len(), "payload out of bounds");
        entries.push((start_epoch, epoch_count, offset, length));
        pos += entry_len;
    }
    Ok(entries)
}

/// Whether `path` names a `.cfxpack` or `.cfxpack.zst` file.
pub fn is_cfxpack(path: &Path) -> bool {
    let name = match path.file_name().and_then(|n| n.to_str()) {
        Some(n) => n,
        None => return false,
    };
    name.ends_with(".cfxpack") || name.ends_with(".cfxpack.zst")
}

/// Whether `path` is a zstd-compressed container (`.cfxpack.zst`).
pub fn is_compressed(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.ends_with(".cfxpack.zst"))
}

/// Strip `.cfxpack` or `.cfxpack.zst` and return the base stem
/// (`epochs_<start>_<end>`).
fn cfxpack_stem(path: &Path) -> Option<&str> {
    let name = path.file_name()?.to_str()?;
    name.strip_suffix(".cfxpack.zst")
        .or_else(|| name.strip_suffix(".cfxpack"))
}

/// The start epoch encoded in a `<prefix>_<start>_<end>.cfxpack[.zst]` file name.
pub fn start_epoch(path: &Path) -> Option<u64> {
    let stem = cfxpack_stem(path)?;
    let mut parts = stem.rsplit('_');
    let _end = parts.next()?;
    parts.next()?.parse().ok()
}

/// The end epoch encoded in a `<prefix>_<start>_<end>.cfxpack[.zst]` file name.
pub fn end_epoch(path: &Path) -> Option<u64> {
    let stem = cfxpack_stem(path)?;
    stem.rsplit('_').next()?.parse().ok()
}
