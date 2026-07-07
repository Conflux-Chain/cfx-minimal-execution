//! Streaming checkpoint I/O — the crate's persistence entry points.
//!
//! `save_state` / `load_state` are the only place that reads or writes the
//! on-disk checkpoint. The wire layout (field order + snapshot framing) is
//! defined once in [`crate::persist::format`]; these functions follow it. The
//! snapshot section is streamed batch-by-batch straight into the snapshot
//! store's bulk-feed entry point, so no byte-keyed `BTreeMap` is ever
//! materialized and the trie itself never touches a reader.

use crate::state::layers::snapshot::SnapshotTrie;
use crate::state::{CommitRoot, State};
use crate::trie::MptValue;
use std::collections::BTreeMap;
use std::io::{self, Read, Write};

/// Stream `state` in the on-disk wire layout defined once in
/// [`crate::persist::format`] (field order + snapshot framing), which
/// [`load_state`] consumes.
pub fn save_state(state: &State, w: &mut impl Write) -> io::Result<()> {
    use crate::persist::wire::{write_len_prefixed, write_u64_le};

    let bin = |e: bincode::Error| io::Error::new(io::ErrorKind::InvalidData, e);

    let expected_entries = state.snapshot_live_entry_count();
    write_u64_le(w, expected_entries as u64)?;
    let mut entries_written = 0usize;
    state.for_each_snapshot_entry(|key, value| {
        write_len_prefixed(w, &key)?;
        write_len_prefixed(w, value)?;
        entries_written += 1;
        Ok(())
    })?;
    if entries_written != expected_entries {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "snapshot entry count changed during write: expected {expected_entries}, wrote {entries_written}"
            ),
        ));
    }

    bincode::serialize_into(&mut *w, state.intermediate_entries()).map_err(bin)?;
    bincode::serialize_into(&mut *w, state.delta_entries()).map_err(bin)?;
    bincode::serialize_into(&mut *w, &state.intermediate_padding_bytes()).map_err(bin)?;
    bincode::serialize_into(&mut *w, &state.delta_padding_bytes()).map_err(bin)?;
    bincode::serialize_into(&mut *w, &state.height()).map_err(bin)?;
    bincode::serialize_into(&mut *w, &state.snapshot_epoch_count()).map_err(bin)?;
    bincode::serialize_into(&mut *w, state.last_root()).map_err(bin)?;

    Ok(())
}

/// Reconstruct a [`State`] by streaming the serialized fields directly from
/// `reader`, following the layout in [`crate::persist::format`]. The snapshot
/// is read batch-by-batch and bulk-fed into the snapshot store without ever
/// materializing the byte-keyed `BTreeMap`; the remaining (small) fields are
/// read via bincode. This is the load-side counterpart to [`save_state`].
pub fn load_state(reader: &mut impl Read) -> io::Result<State> {
    use crate::persist::wire::{read_len_prefixed, read_u64_le};

    let bin = |e: bincode::Error| io::Error::new(io::ErrorKind::InvalidData, e);
    let timing = crate::diag::recover_timing();

    // 1. snapshot — read batch-by-batch and bulk-fed into the snapshot store.
    // On-disk keys are ascending, so build the store densely: read fixed-size
    // sorted batches of `W` and hand each to the snapshot's `bulk_extend_sorted`
    // (dense level-2 leaves) instead of monotonic per-entry `insert` (~half-full
    // leaves). The snapshot store stays oblivious to the reader; peak transient
    // is one batch. `W` is env-tunable for profiling.
    let t = std::time::Instant::now();
    let count = read_u64_le(reader)? as usize;
    let w = crate::diag::bulk_w();
    let mut snapshot = SnapshotTrie::default();
    let mut batch: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(w.min(count));
    for _ in 0..count {
        let key = read_len_prefixed(reader)?;
        let value = read_len_prefixed(reader)?;
        batch.push((key, value));
        if batch.len() >= w {
            snapshot.bulk_extend_sorted(std::mem::replace(&mut batch, Vec::with_capacity(w)));
        }
    }
    if !batch.is_empty() {
        snapshot.bulk_extend_sorted(batch);
    }
    if timing {
        eprintln!(
            "[recover] stream_read_snapshot entries={} elapsed={}ms",
            snapshot.len(),
            t.elapsed().as_millis()
        );
    }

    // 2-3. intermediate / delta — small, plain bincode.
    let intermediate: BTreeMap<Vec<u8>, MptValue> =
        bincode::deserialize_from(&mut *reader).map_err(bin)?;
    let delta: BTreeMap<Vec<u8>, MptValue> =
        bincode::deserialize_from(&mut *reader).map_err(bin)?;
    // 4-8. scalar tail.
    let intermediate_mpt_key_padding: [u8; 32] =
        bincode::deserialize_from(&mut *reader).map_err(bin)?;
    let delta_mpt_key_padding: [u8; 32] =
        bincode::deserialize_from(&mut *reader).map_err(bin)?;
    let height: u64 = bincode::deserialize_from(&mut *reader).map_err(bin)?;
    let snapshot_epoch_count: u32 = bincode::deserialize_from(&mut *reader).map_err(bin)?;
    let last_root: Option<CommitRoot> = bincode::deserialize_from(&mut *reader).map_err(bin)?;

    Ok(State::from_parts(
        snapshot,
        intermediate,
        delta,
        intermediate_mpt_key_padding,
        delta_mpt_key_padding,
        height,
        snapshot_epoch_count,
        last_root,
    ))
}
