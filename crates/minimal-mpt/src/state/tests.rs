use super::*;
use crate::{Space, StorageKey};

fn key(byte: u8) -> StorageKeyWithSpace {
    StorageKeyWithSpace {
        key: StorageKey::Account(vec![byte; 20]),
        space: Space::Native,
    }
}

#[test]
fn get_set_commit() {
    let mut state = State::new();
    state.set(key(1), Box::from([9u8])).unwrap();
    assert_eq!(state.get(key(1)).unwrap().unwrap().as_ref(), &[9u8]);
    let root = state.commit().unwrap();
    assert_ne!(root.delta_root, MERKLE_NULL_NODE);
}

#[test]
fn tombstone_hides_value() {
    let mut snapshot = BTreeMap::new();
    snapshot.insert(key(1).to_key_bytes().unwrap(), Box::from([1u8]));
    let mut state = State::from_snapshot(snapshot);
    assert_eq!(state.get(key(1)).unwrap().unwrap().as_ref(), &[1u8]);
    state.set(key(1), Box::new([])).unwrap();
    assert!(state.get(key(1)).unwrap().is_none());
}

#[test]
fn address_prefix_reads_delta_after_canonical_filter() {
    let mut state = State::new();
    state.set(key(1), Box::from([1u8])).unwrap();
    state.set(key(2), Box::from([2u8])).unwrap();
    let prefix = StorageKeyWithSpace {
        key: StorageKey::AddressPrefix(vec![1]),
        space: Space::Native,
    };
    let values = state.get_all_by_prefix(prefix).unwrap().unwrap();
    assert_eq!(values.len(), 1);
    assert_eq!(values[0].0, key(1).to_key_bytes().unwrap());
}

#[test]
fn address_prefix_delete_removes_unrelated_delta_keys() {
    let mut state = State::new();
    state.set(key(1), Box::from([1u8])).unwrap();
    state.set(key(2), Box::from([2u8])).unwrap();
    let prefix = StorageKeyWithSpace {
        key: StorageKey::AddressPrefix(vec![1]),
        space: Space::Native,
    };
    let values = state.delete_all_by_prefix(prefix).unwrap().unwrap();
    assert_eq!(values.len(), 1);
    assert!(state.get(key(1)).unwrap().is_none());
    assert!(state.get(key(2)).unwrap().is_none());
}

#[test]
fn delete_all_prefix_hides_snapshot_values() {
    let mut snapshot = BTreeMap::new();
    snapshot.insert(key(1).to_key_bytes().unwrap(), Box::from([1u8]));
    let mut state = State::from_snapshot(snapshot);
    let deleted = state
        .delete_all_by_prefix(StorageKeyWithSpace {
            key: StorageKey::AddressPrefix(vec![1]),
            space: Space::Native,
        })
        .unwrap()
        .unwrap();
    assert_eq!(deleted.len(), 1);
    assert!(state.get(key(1)).unwrap().is_none());
}

#[test]
fn manager_persists_latest_state() {
    // Persist a committed state via the streaming writer, read it back, and
    // assert the observable (the value under key(2)) and the full persisted
    // image round-trip identically.
    let mut state = State::new();
    state.set(key(2), Box::from([7u8])).unwrap();
    state.commit().unwrap();

    let mut bytes = Vec::new();
    state.write_streaming(&mut bytes).unwrap();
    let restored = State::from_reader(&mut bytes.as_slice()).unwrap();

    assert_eq!(restored.get(key(2)).unwrap().unwrap().as_ref(), &[7u8]);
    assert_eq!(restored.persisted(), state.persisted());
}

#[test]
fn commit_produces_nonnull_delta_root() {
    let mut state = State::new();
    state.set(key(3), Box::from([8u8])).unwrap();
    let root = state.commit().unwrap();
    assert_ne!(root.delta_root, MERKLE_NULL_NODE);
}

#[test]
fn write_streaming_roundtrips_persisted_state() {
    // `snapshot_epoch_count(1)` rotates on every commit, so committed data
    // migrates into the intermediate (and then snapshot) layer naturally — no
    // `put_intermediate_raw` needed. Two commits leave a populated snapshot and
    // a populated intermediate; a final uncommitted set populates the delta.
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

    let mut bytes = Vec::new();
    state.write_streaming(&mut bytes).unwrap();
    let restored = State::from_reader(&mut bytes.as_slice()).unwrap();

    assert_eq!(restored.persisted(), state.persisted());
}

#[test]
fn deterministic_random_set_get_commit_sequence() {
    let mut rng = Lcg::new(0x13_57_9b_df);
    let mut state = State::new();
    let mut expected = BTreeMap::<u8, Option<Vec<u8>>>::new();

    for i in 0..5_000 {
        let id = (rng.next() % 96) as u8;
        match rng.next() % 5 {
            0 => {
                state.set(key(id), Box::new([])).unwrap();
                expected.insert(id, None);
            }
            1..=3 => {
                let value = vec![id, i as u8, (i >> 8) as u8];
                state
                    .set(key(id), value.clone().into_boxed_slice())
                    .unwrap();
                expected.insert(id, Some(value));
            }
            _ => {
                let actual = state.get(key(id)).unwrap().map(|v| v.to_vec());
                let expected_value = expected.get(&id).cloned().flatten();
                assert_eq!(actual, expected_value, "id={id}, i={i}");
            }
        }
        if i % 97 == 0 {
            let root_a = state.commit().unwrap();
            let root_b = state.commit().unwrap();
            assert_eq!(root_a, root_b);
        }
    }
}

#[derive(Clone)]
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
        self.0
    }
}
