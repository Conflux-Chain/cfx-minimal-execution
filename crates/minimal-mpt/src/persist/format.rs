//! **Single source of truth for the on-disk checkpoint format.**
//!
//! Everything that pins the checkpoint byte layout is defined here: the
//! persisted field set and their order ([`PersistedState`]), and the streaming
//! framing spec below. The actual read/write *functions*
//! (`State::write_streaming` / `State::from_reader`) still live in `state/` (they
//! move to `persist::stream` in a later commit), but they are required to follow
//! the layout defined here — do not describe the format anywhere else.
//!
//! ## Wire layout (produced by `State::write_streaming`, consumed by `State::from_reader`)
//!
//! The checkpoint has **no magic bytes and no version tag** — it is a bare
//! concatenation of the [`PersistedState`] fields, in declaration order, using
//! bincode 1.x default (little-endian, fixint) encoding:
//!
//! 1. `snapshot`   — streamed framing: a LE `u64` live-entry count, then that
//!    many `(key, value)` pairs, each a bincode length-prefixed byte string
//!    (`u64` len + raw bytes). This is byte-compatible with bincode's own
//!    `BTreeMap<Vec<u8>, Box<[u8]>>` encoding, but written entry-by-entry so the
//!    (dominant) snapshot never materializes as a `BTreeMap` on save/load.
//! 2. `intermediate` — bincode `BTreeMap<Vec<u8>, MptValue>`.
//! 3. `delta`        — bincode `BTreeMap<Vec<u8>, MptValue>`.
//! 4. `intermediate_mpt_key_padding` — bincode `[u8; 32]`.
//! 5. `delta_mpt_key_padding`        — bincode `[u8; 32]`.
//! 6. `height`             — bincode `u64`.
//! 7. `snapshot_epoch_count` — bincode `u32`.
//! 8. `last_root`          — bincode `Option<CommitRoot>`.
//!
//! Reordering a field, changing an encoding, or altering the `MptValue` variant
//! layout is a checkpoint-format break. The tests below (the `MptValue` golden
//! byte pins and the save→load→save byte-identity guard) exist to catch any such
//! change.

use crate::{state::CommitRoot, trie::MptValue};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The complete, order-significant set of fields that constitute a persisted
/// checkpoint. Field declaration order **is** the on-disk order (see the module
/// docs) — do not reorder.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedState {
    pub snapshot: BTreeMap<Vec<u8>, Box<[u8]>>,
    pub intermediate: BTreeMap<Vec<u8>, MptValue>,
    pub delta: BTreeMap<Vec<u8>, MptValue>,
    pub intermediate_mpt_key_padding: [u8; 32],
    pub delta_mpt_key_padding: [u8; 32],
    pub height: u64,
    pub snapshot_epoch_count: u32,
    pub last_root: Option<CommitRoot>,
}

impl Default for PersistedState {
    fn default() -> Self {
        Self {
            snapshot: BTreeMap::new(),
            intermediate: BTreeMap::new(),
            delta: BTreeMap::new(),
            intermediate_mpt_key_padding: crate::DeltaMptKeyPadding::genesis().0,
            delta_mpt_key_padding: crate::DeltaMptKeyPadding::genesis().0,
            height: 0,
            snapshot_epoch_count: crate::state::DEFAULT_SNAPSHOT_EPOCH_COUNT,
            last_root: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden byte-pin for the `MptValue` wire encoding. `MptValue` is the value
    /// type stored in the `intermediate` and `delta` maps, so its encoding is
    /// part of the checkpoint format. bincode 1.x encodes an enum as an LE `u32`
    /// variant index followed by the variant payload, and a `Box<[u8]>` as an LE
    /// `u64` length prefix followed by the raw bytes. Any accidental change to
    /// the variant order, the length encoding, or the derive would break these
    /// hardcoded goldens.
    #[test]
    fn mpt_value_bincode_encoding_matches_legacy_mpt_value_disk() {
        // Variant 0 = Some(bytes): u32 tag `0` + u64 len + raw bytes.
        let cases: &[(MptValue, Vec<u8>)] = &[
            // Some([1, 2, 3])
            (
                MptValue::Some(Box::from([1u8, 2, 3])),
                {
                    let mut v = Vec::new();
                    v.extend_from_slice(&0u32.to_le_bytes());
                    v.extend_from_slice(&3u64.to_le_bytes());
                    v.extend_from_slice(&[1u8, 2, 3]);
                    v
                },
            ),
            // Some([]) — present-but-empty is distinct from a tombstone.
            (
                MptValue::Some(Box::from([])),
                {
                    let mut v = Vec::new();
                    v.extend_from_slice(&0u32.to_le_bytes());
                    v.extend_from_slice(&0u64.to_le_bytes());
                    v
                },
            ),
            // Some(0xff) — single high byte, guards against sign/width slips.
            (
                MptValue::Some(Box::from([0xffu8])),
                {
                    let mut v = Vec::new();
                    v.extend_from_slice(&0u32.to_le_bytes());
                    v.extend_from_slice(&1u64.to_le_bytes());
                    v.push(0xff);
                    v
                },
            ),
            // Variant 1 = Tombstone: u32 tag `1`, no payload.
            (MptValue::Tombstone, 1u32.to_le_bytes().to_vec()),
        ];

        for (value, expected) in cases {
            let actual = bincode::serialize(value).unwrap();
            assert_eq!(
                &actual, expected,
                "MptValue wire encoding changed for {value:?}"
            );
            // And the pinned bytes must round-trip back to the same value.
            let rt: MptValue = bincode::deserialize(expected).unwrap();
            assert_eq!(&rt, value, "MptValue did not round-trip from pinned bytes");
        }
    }

    /// **Permanent checkpoint-format guard.** Build a small but non-trivial
    /// `State` with all three layers (snapshot, intermediate, delta) populated,
    /// then `save → load → save` and assert the two serialized images are byte
    /// identical, and that the reloaded state's `persisted()` image equals the
    /// original. If a future change (e.g. the c8/c9 move of the I/O functions out
    /// of `State`) perturbs the on-disk bytes, this fails.
    #[test]
    fn golden_save_load_save_is_byte_identical() {
        use crate::{Space, State, StateTrait, StorageKey, StorageKeyWithSpace};

        fn key(byte: u8) -> StorageKeyWithSpace {
            StorageKeyWithSpace {
                key: StorageKey::Account(vec![byte; 20]),
                space: Space::Native,
            }
        }

        // `snapshot_epoch_count(1)` rotates on every commit, so committed data
        // migrates into intermediate then snapshot naturally. Two commits leave a
        // populated snapshot and intermediate; a final uncommitted set populates
        // the delta — exercising all three layers, mirroring
        // `write_streaming_roundtrips_persisted_state`.
        let mut state = State::with_snapshot_epoch_count(1);
        state.set(key(1), Box::from([1u8, 2])).unwrap();
        state.commit().unwrap(); // key(1) -> intermediate
        state.set(key(2), Box::from([3u8, 4])).unwrap();
        state.commit().unwrap(); // key(1) -> snapshot, key(2) -> intermediate
        state.set(key(3), Box::from([6u8, 7, 8])).unwrap(); // key(3) stays in delta

        let before = state.persisted();
        assert!(!before.snapshot.is_empty(), "snapshot layer must be exercised");
        assert!(
            !before.intermediate.is_empty(),
            "intermediate layer must be exercised"
        );
        assert!(!before.delta.is_empty(), "delta layer must be exercised");

        // save
        let mut bytes1 = Vec::new();
        state.write_streaming(&mut bytes1).unwrap();
        // load
        let restored = State::from_reader(&mut bytes1.as_slice()).unwrap();
        // save again
        let mut bytes2 = Vec::new();
        restored.write_streaming(&mut bytes2).unwrap();

        assert_eq!(
            bytes1, bytes2,
            "checkpoint bytes changed across save→load→save"
        );
        assert_eq!(
            restored.persisted(),
            state.persisted(),
            "reloaded persisted image diverged from original"
        );
    }
}
