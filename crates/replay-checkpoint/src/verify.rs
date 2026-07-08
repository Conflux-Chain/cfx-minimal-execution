//! Verification that the streaming path produces identical results to the
//! legacy full-materialization path.

use crate::legacy::Checkpoint;
use anyhow::{Context, Result};
use std::path::Path;

/// Verify that `load` → `save_full_bincode` and `load` → `rebuild_and_save_streaming_for_verify` both produce
/// byte-identical output to the original file. Returns `(full_ok, stream_ok)`.
pub fn verify_roundtrip(path: &Path) -> Result<(bool, bool)> {
    let original = std::fs::read(path)
        .with_context(|| format!("read {}", path.display()))?;
    eprintln!("original: {} bytes", original.len());

    let ckpt = Checkpoint::load(path)?
        .context("no checkpoint")?;
    eprintln!("loaded: height={}", ckpt.height());

    let dir = tempfile::tempdir().context("tempdir")?;

    // Full save round-trip
    let full_path = dir.path().join("full.bin");
    ckpt.save_full_bincode(&full_path).context("save full")?;
    let resaved_full = std::fs::read(&full_path).context("read resaved full")?;
    eprintln!("resaved (full): {} bytes", resaved_full.len());

    let full_ok = if original == resaved_full {
        eprintln!("PASS: full save is byte-identical to original");
        true
    } else {
        eprintln!("FAIL: full save differs from original");
        eprintln!("  original len={} resaved len={}", original.len(), resaved_full.len());
        if let Some(pos) = original.iter().zip(resaved_full.iter())
            .position(|(a, b)| a != b)
        {
            eprintln!("  first diff at byte {}: orig=0x{:02x} resaved=0x{:02x}",
                pos, original[pos], resaved_full[pos]);
        }
        false
    };

    // Streaming save round-trip
    let stream_path = dir.path().join("stream.bin");
    ckpt.rebuild_and_save_streaming_for_verify(&stream_path).context("save streaming")?;
    let resaved_stream = std::fs::read(&stream_path).context("read resaved stream")?;
    eprintln!("resaved (streaming): {} bytes", resaved_stream.len());

    let stream_ok = if original == resaved_stream {
        eprintln!("PASS: streaming save is byte-identical to original");
        true
    } else {
        eprintln!("FAIL: streaming save differs from original");
        if let Some(pos) = original.iter().zip(resaved_stream.iter())
            .position(|(a, b)| a != b)
        {
            eprintln!("  first diff at byte {}: orig=0x{:02x} resaved=0x{:02x}",
                pos, original[pos], resaved_stream[pos]);
        }
        false
    };

    Ok((full_ok, stream_ok))
}

/// Verify that `load_streaming` reconstructs exactly what `load` + `into_data`
/// does. Compares the materialized `PersistedState` field-by-field.
pub fn verify_streaming_matches(path: &Path) -> Result<bool> {
    let full = Checkpoint::load(path)?.context("no checkpoint at path")?;
    let ref_data = full.into_data()?;
    let ref_mmpt = ref_data.state.persisted();

    let restored = crate::CheckpointData::load(path)?
        .context("load_streaming None")?;
    let stream_mmpt = restored.state.persisted();

    let mut ok = true;
    if ref_mmpt != stream_mmpt {
        ok = false;
        eprintln!("FAIL: mmpt differs");
        if ref_mmpt.snapshot != stream_mmpt.snapshot {
            eprintln!(
                "  snapshot: ref={} stream={}",
                ref_mmpt.snapshot.len(),
                stream_mmpt.snapshot.len()
            );
            for (k, v) in ref_mmpt.snapshot.iter() {
                match stream_mmpt.snapshot.get(k) {
                    None => {
                        eprintln!("  key missing in stream: {:02x?}", &k[..k.len().min(8)]);
                        break;
                    }
                    Some(sv) if sv != v => {
                        eprintln!("  value differs at {:02x?}", &k[..k.len().min(8)]);
                        break;
                    }
                    _ => {}
                }
            }
        }
        if ref_mmpt.intermediate != stream_mmpt.intermediate {
            eprintln!("  intermediate differs");
        }
        if ref_mmpt.delta != stream_mmpt.delta {
            eprintln!("  delta differs");
        }
        if ref_mmpt.height != stream_mmpt.height {
            eprintln!("  height {} vs {}", ref_mmpt.height, stream_mmpt.height);
        }
        if ref_mmpt.last_root != stream_mmpt.last_root {
            eprintln!("  last_root differs");
        }
        if ref_mmpt.intermediate_mpt_key_padding != stream_mmpt.intermediate_mpt_key_padding
            || ref_mmpt.delta_mpt_key_padding != stream_mmpt.delta_mpt_key_padding
        {
            eprintln!("  padding differs");
        }
        if ref_mmpt.snapshot_epoch_count != stream_mmpt.snapshot_epoch_count {
            eprintln!("  epoch_count differs");
        }
    }
    if ref_data.height != restored.height {
        ok = false;
        eprintln!("FAIL: height {} vs {}", ref_data.height, restored.height);
    }
    if ref_data.previous_epoch_hash != restored.previous_epoch_hash {
        ok = false;
        eprintln!("FAIL: previous_epoch_hash differs");
    }
    if ref_data.previous_state_root != restored.previous_state_root {
        ok = false;
        eprintln!("FAIL: previous_state_root differs");
    }
    if ref_data.previous_epoch_pos_view != restored.previous_epoch_pos_view {
        ok = false;
        eprintln!("FAIL: previous_epoch_pos_view differs");
    }
    if ref_data.previous_epoch_finalized_epoch != restored.previous_epoch_finalized_epoch {
        ok = false;
        eprintln!("FAIL: previous_epoch_finalized_epoch differs");
    }
    if ref_data.commitments.len() != restored.commitments.len() {
        ok = false;
        eprintln!("FAIL: commitments count differs");
    }
    if ref_data.executed_epochs.len() != restored.executed_epochs.len() {
        ok = false;
        eprintln!("FAIL: executed_epochs count differs");
    }

    eprintln!(
        "height={} snapshot={} intermediate={} delta={} commitments={} executed={}",
        ref_data.height,
        stream_mmpt.snapshot.len(),
        stream_mmpt.intermediate.len(),
        stream_mmpt.delta.len(),
        restored.commitments.len(),
        restored.executed_epochs.len(),
    );
    Ok(ok)
}
