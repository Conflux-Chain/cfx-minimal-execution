//! Offline blame labeller: scan `.cfxpack` containers for blame information,
//! determine which pivots are state-invalid, and set `FLAG_BLAMED` (bit 7) on
//! those pivot records, renaming the container to `.cfxpackb` (= certified).

use anyhow::{bail, ensure, Context, Result};
use cfxpack::container;
use cfxpack::decode::{block_flag_sites, decode_packet_ext};
use cfxpack::packet::{FLAG_BLAMED, FLAG_PIVOT};
use clap::Parser;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Parser)]
#[command(name = "label_blame")]
struct Cli {
    #[arg(long)]
    input_dir: PathBuf,
    /// Clean-window size for anchor discovery. Abort if any blame >= this value.
    #[arg(long, default_value_t = 10000)]
    window: u64,
    /// Print what would be done without modifying any files.
    #[arg(long, default_value_t = false)]
    dry_run: bool,
}

#[derive(Debug)]
struct PivotInfo {
    height: u64,
    blame: u64,
}

#[derive(Debug)]
struct Storm {
    start: u64,
    end: u64,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let files = container::collect_files(&cli.input_dir)?;
    eprintln!("collected {} container files", files.len());

    // Phase 1: Collect all pivot (height, blame) pairs in epoch order.
    let pivots = collect_pivots(&files)?;
    eprintln!("collected {} pivots", pivots.len());
    if pivots.is_empty() {
        bail!("no pivots found");
    }

    // Phase 2: Find the anchor — highest clean window of W consecutive blame=0.
    let anchor = find_anchor(&pivots, cli.window)?;
    eprintln!(
        "anchor: height {} (clean window of {} consecutive blame=0 pivots ending here)",
        anchor, cli.window,
    );

    // Phase 3: Walk down from anchor, marking invalid pivots.
    let invalid_set = walk_blame(&pivots, anchor, cli.window)?;

    // Phase 4: Report storms.
    let storms = find_storms(&invalid_set, &pivots);
    let max_blame = pivots.iter().map(|p| p.blame).max().unwrap_or(0);
    eprintln!("flagged pivots: {}", invalid_set.len());
    eprintln!("storms: {}", storms.len());
    for s in &storms {
        eprintln!("  storm {}..={} (length {})", s.start, s.end, s.end - s.start + 1);
    }
    eprintln!("max blame: {}", max_blame);

    // Determine which files are fully below/at the anchor (will be labelled).
    let first_height = pivots.first().unwrap().height;
    let last_height = pivots.last().unwrap().height;
    let unlabelled_start = if anchor < last_height { anchor + 1 } else { last_height + 1 };
    eprintln!(
        "labelling range: {}..={}, unlabelled tail: {}..={}",
        first_height, anchor, unlabelled_start, last_height,
    );

    if cli.dry_run {
        eprintln!("dry-run: no files modified");
        return Ok(());
    }

    // Phase 5: Apply flags and rename.
    let mut flagged_count = 0u64;
    for path in &files {
        let file_end = container::end_epoch(path).unwrap_or(u64::MAX);
        if file_end > anchor {
            eprintln!("skip (tail): {}", path.display());
            continue;
        }
        if container::is_labelled(path) {
            eprintln!("skip (already labelled): {}", path.display());
            continue;
        }
        let n = patch_and_rename(path, &invalid_set)?;
        flagged_count += n;
        eprintln!("labelled: {} (flagged {} pivots)", path.display(), n);
    }
    eprintln!("total flagged pivots written: {}", flagged_count);
    Ok(())
}

fn collect_pivots(files: &[PathBuf]) -> Result<Vec<PivotInfo>> {
    let mut pivots = Vec::new();
    for path in files {
        let data = read_container(path)?;
        let entries = container::parse_directory(&data)
            .with_context(|| format!("parse {}", path.display()))?;
        let pos_h = u64::MAX;
        for (start_epoch, _epoch_count, offset, length) in entries {
            let packet = decode_packet_ext(&data[offset..offset + length], pos_h)
                .with_context(|| format!("decode group at epoch {} in {}", start_epoch, path.display()))?;
            for block in &packet.blocks {
                if block.flags & FLAG_PIVOT != 0 {
                    pivots.push(PivotInfo {
                        height: block.height,
                        blame: block.blame,
                    });
                }
            }
        }
    }
    pivots.sort_by_key(|p| p.height);
    // Verify height continuity.
    for w in pivots.windows(2) {
        ensure!(
            w[1].height == w[0].height + 1,
            "pivot height gap: {} -> {}",
            w[0].height,
            w[1].height,
        );
    }
    Ok(pivots)
}

fn find_anchor(pivots: &[PivotInfo], window: u64) -> Result<u64> {
    let w = window as usize;
    ensure!(pivots.len() >= w, "fewer pivots ({}) than window ({})", pivots.len(), w);

    let mut best_end: Option<usize> = None;
    let mut run = 0usize;
    for (i, p) in pivots.iter().enumerate() {
        if p.blame == 0 {
            run += 1;
            if run >= w {
                best_end = Some(i);
            }
        } else {
            run = 0;
        }
    }
    let end_idx = best_end.ok_or_else(|| {
        anyhow::anyhow!("no clean window of {} consecutive blame=0 pivots found", window)
    })?;
    Ok(pivots[end_idx].height)
}

fn walk_blame(pivots: &[PivotInfo], anchor: u64, window: u64) -> Result<BTreeSet<u64>> {
    let first = pivots.first().unwrap().height;
    let by_height: BTreeMap<u64, u64> = pivots
        .iter()
        .map(|p| (p.height, p.blame))
        .collect();

    let mut invalid = BTreeSet::new();
    let mut cur = anchor;

    while cur >= first {
        let blame = *by_height.get(&cur).unwrap();
        ensure!(
            blame < window,
            "pivot height {} has blame {} >= window {}; increase --window",
            cur, blame, window,
        );

        if blame == 0 {
            if cur == first {
                break;
            }
            cur -= 1;
            continue;
        }

        // cur is valid with blame=k: mark cur-1..=cur-k as invalid.
        for offset in 1..=blame {
            let h = cur.checked_sub(offset).unwrap_or(0);
            if h >= first {
                invalid.insert(h);
            }
        }

        // Jump to cur-k-1 (the next valid pivot).
        match cur.checked_sub(blame + 1) {
            Some(next) if next >= first => cur = next,
            _ => break,
        }
    }

    // Boundary check: nothing above the anchor may blame down INTO the
    // anchored range. Invalid blocks carry garbage blame=0, so a storm's
    // leading edge can extend an apparent clean run — the walk legitimately
    // marks such trailing "clean" pivots invalid. What would actually poison
    // the labels is a (potentially valid) header above the anchor whose blame
    // interval reaches the anchor or below: the anchor window itself might
    // then sit inside a storm onset.
    for p in pivots.iter().filter(|p| p.height > anchor && p.blame > 0) {
        ensure!(
            p.height.saturating_sub(p.blame) > anchor,
            "pivot {} (above anchor {}) blames down to {}: the anchor window \
             may sit inside a storm onset. The input likely ends mid-storm — \
             extend the data or re-run once more epochs are extracted",
            p.height,
            anchor,
            p.height.saturating_sub(p.blame),
        );
    }

    // Sanity: blame values on invalid pivots may be garbage (but tolerated).
    // Blame > u32::MAX on a non-invalid pivot is suspicious.
    for p in pivots.iter() {
        if p.blame > u32::MAX as u64 && !invalid.contains(&p.height) {
            bail!(
                "pivot height {} has suspicious blame {} (> u32::MAX) but is not marked invalid",
                p.height, p.blame,
            );
        }
    }

    Ok(invalid)
}

fn find_storms(invalid: &BTreeSet<u64>, _pivots: &[PivotInfo]) -> Vec<Storm> {
    if invalid.is_empty() {
        return Vec::new();
    }
    let mut storms = Vec::new();
    let sorted: Vec<u64> = invalid.iter().copied().collect();
    let mut start = sorted[0];
    let mut end = sorted[0];
    for &h in &sorted[1..] {
        if h == end + 1 {
            end = h;
        } else {
            storms.push(Storm { start, end });
            start = h;
            end = h;
        }
    }
    storms.push(Storm { start, end });
    storms
}

fn patch_and_rename(path: &Path, invalid: &BTreeSet<u64>) -> Result<u64> {
    let compressed = container::is_compressed(path);
    let raw = std::fs::read(path)
        .with_context(|| format!("read {}", path.display()))?;

    let mut data = if compressed {
        let mut decoder = ruzstd::StreamingDecoder::new(raw.as_slice())
            .map_err(|e| anyhow::anyhow!("zstd init: {e}"))?;
        let mut out = Vec::with_capacity(raw.len() * 3);
        std::io::Read::read_to_end(&mut decoder, &mut out)
            .with_context(|| format!("zstd decompress {}", path.display()))?;
        out
    } else {
        raw
    };

    let entries = container::parse_directory(&data)?;
    let mut flagged = 0u64;

    for (_start_epoch, _epoch_count, offset, length) in &entries {
        let packet_data = &data[*offset..*offset + *length];
        let sites = block_flag_sites(packet_data)?;
        let pos_h = u64::MAX;
        let packet = decode_packet_ext(packet_data, pos_h)?;
        for block in &packet.blocks {
            if block.flags & FLAG_PIVOT == 0 {
                continue;
            }
            if invalid.contains(&block.height) {
                let (flag_offset, _) = sites
                    .iter()
                    .find(|(_, hash)| *hash == block.hash)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "no flag site for invalid pivot {} ({:?})",
                            block.height,
                            block.hash,
                        )
                    })?;
                data[offset + flag_offset] |= FLAG_BLAMED;
                flagged += 1;
            }
        }
    }

    // Write as uncompressed .cfxpackb (if the input was .zst, the output is
    // the decompressed + patched version; the user can recompress externally).
    let new_path = labelled_path(path);
    write_atomic(&new_path, &data)?;

    // Remove the original (different name).
    if new_path != *path {
        std::fs::remove_file(path)
            .with_context(|| format!("remove original {}", path.display()))?;
    }

    Ok(flagged)
}

/// Always produces `.cfxpackb` (uncompressed), even if the input was `.zst`.
fn labelled_path(path: &Path) -> PathBuf {
    let name = path.file_name().unwrap().to_str().unwrap();
    let new_name = if name.ends_with(".cfxpack.zst") {
        name.replace(".cfxpack.zst", ".cfxpackb")
    } else if name.ends_with(".cfxpack") {
        name.replace(".cfxpack", ".cfxpackb")
    } else {
        return path.to_path_buf();
    };
    path.with_file_name(new_name)
}

fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    {
        let mut f = std::fs::File::create(&tmp)
            .with_context(|| format!("create {}", tmp.display()))?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename into {}", path.display()))?;
    Ok(())
}

fn read_container(path: &Path) -> Result<Vec<u8>> {
    let raw = std::fs::read(path)
        .with_context(|| format!("read {}", path.display()))?;
    if container::is_compressed(path) {
        let mut decoder = ruzstd::StreamingDecoder::new(raw.as_slice())
            .map_err(|e| anyhow::anyhow!("zstd init: {e}"))?;
        let mut out = Vec::with_capacity(raw.len() * 3);
        std::io::Read::read_to_end(&mut decoder, &mut out)
            .with_context(|| format!("zstd decompress {}", path.display()))?;
        Ok(out)
    } else {
        Ok(raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walk_pure_clean_chain() {
        let pivots: Vec<PivotInfo> = (100..200)
            .map(|h| PivotInfo { height: h, blame: 0 })
            .collect();
        let invalid = walk_blame(&pivots, 199, 50).unwrap();
        assert!(invalid.is_empty());
    }

    #[test]
    fn walk_single_storm() {
        // Heights 100..120. Height 115 has blame=3 (meaning 114,113,112 invalid).
        let mut pivots: Vec<PivotInfo> = (100..120)
            .map(|h| PivotInfo { height: h, blame: 0 })
            .collect();
        pivots[15].blame = 3; // height 115
        // Heights 112,113,114 are invalid; their blame values are garbage.
        pivots[12].blame = 999;
        pivots[13].blame = 888;
        pivots[14].blame = 777;

        let invalid = walk_blame(&pivots, 119, 10000).unwrap();
        assert_eq!(invalid.len(), 3);
        assert!(invalid.contains(&112));
        assert!(invalid.contains(&113));
        assert!(invalid.contains(&114));
    }

    #[test]
    fn walk_multi_storm() {
        // Heights 100..130. Storm 1: 115 blames 3 (112-114 invalid).
        // Storm 2: 125 blames 2 (123-124 invalid).
        let mut pivots: Vec<PivotInfo> = (100..130)
            .map(|h| PivotInfo { height: h, blame: 0 })
            .collect();
        pivots[15].blame = 3; // height 115
        pivots[25].blame = 2; // height 125

        let invalid = walk_blame(&pivots, 129, 10000).unwrap();
        assert_eq!(invalid.len(), 5);
        assert!(invalid.contains(&112));
        assert!(invalid.contains(&113));
        assert!(invalid.contains(&114));
        assert!(invalid.contains(&123));
        assert!(invalid.contains(&124));
    }

    #[test]
    fn walk_blame_ge_window_aborts() {
        let mut pivots: Vec<PivotInfo> = (100..200)
            .map(|h| PivotInfo { height: h, blame: 0 })
            .collect();
        pivots[50].blame = 10; // height 150, blame = 10 >= window 10
        let result = walk_blame(&pivots, 199, 10);
        assert!(result.is_err());
    }

    #[test]
    fn find_anchor_picks_highest_window() {
        // Clean everywhere except a storm at 150-153.
        let mut pivots: Vec<PivotInfo> = (100..200)
            .map(|h| PivotInfo { height: h, blame: 0 })
            .collect();
        pivots[50].blame = 1; // height 150
        pivots[51].blame = 1; // height 151
        // The highest clean window of 10 ends at 199.
        let anchor = find_anchor(&pivots, 10).unwrap();
        assert_eq!(anchor, 199);
    }

    #[test]
    fn find_storms_groups_contiguous() {
        let mut invalid = BTreeSet::new();
        invalid.insert(10);
        invalid.insert(11);
        invalid.insert(12);
        invalid.insert(20);
        invalid.insert(21);

        let storms = find_storms(&invalid, &[]);
        assert_eq!(storms.len(), 2);
        assert_eq!(storms[0].start, 10);
        assert_eq!(storms[0].end, 12);
        assert_eq!(storms[1].start, 20);
        assert_eq!(storms[1].end, 21);
    }

    #[test]
    fn labelled_path_transforms() {
        assert_eq!(
            labelled_path(Path::new("/data/epochs_100_200.cfxpack")),
            PathBuf::from("/data/epochs_100_200.cfxpackb"),
        );
        assert_eq!(
            labelled_path(Path::new("/data/epochs_100_200.cfxpack.zst")),
            PathBuf::from("/data/epochs_100_200.cfxpackb"),
        );
    }
}
