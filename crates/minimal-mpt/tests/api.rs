use cfx_minimal_mpt::{
    DeltaMptKeyPadding, FileStore, MemoryStore, MptValueDisk, PersistedState, Space, StateManager,
    StateTrait, StorageKey, StorageKeyWithSpace, H256, MERKLE_NULL_NODE,
};
use std::fs;

fn account(byte: u8, space: Space) -> StorageKeyWithSpace {
    StorageKeyWithSpace {
        key: StorageKey::Account(vec![byte; 20]),
        space,
    }
}

fn storage(address_byte: u8, storage_key: Vec<u8>, space: Space) -> StorageKeyWithSpace {
    StorageKeyWithSpace {
        key: StorageKey::Storage {
            address: vec![address_byte; 20],
            storage_key,
        },
        space,
    }
}

fn all_key_shapes(byte: u8, space: Space) -> Vec<StorageKeyWithSpace> {
    let address = vec![byte; 20];
    vec![
        StorageKeyWithSpace {
            key: StorageKey::Account(address.clone()),
            space,
        },
        StorageKeyWithSpace {
            key: StorageKey::StorageRoot(address.clone()),
            space,
        },
        StorageKeyWithSpace {
            key: StorageKey::Storage {
                address: address.clone(),
                storage_key: vec![byte ^ 0x55; 32],
            },
            space,
        },
        StorageKeyWithSpace {
            key: StorageKey::CodeRoot(address.clone()),
            space,
        },
        StorageKeyWithSpace {
            key: StorageKey::Code {
                address: address.clone(),
                code_hash: vec![byte ^ 0xaa; 32],
            },
            space,
        },
        StorageKeyWithSpace {
            key: StorageKey::DepositList(address.clone()),
            space,
        },
        StorageKeyWithSpace {
            key: StorageKey::VoteList(address),
            space,
        },
    ]
}

#[test]
fn public_api_roundtrip_native_and_ethereum() {
    let mut manager = StateManager::new(MemoryStore::new()).unwrap();
    manager
        .set(account(1, Space::Native), Box::from([10u8]))
        .unwrap();
    manager
        .set(account(1, Space::Ethereum), Box::from([11u8]))
        .unwrap();

    assert_eq!(
        manager
            .get(account(1, Space::Native))
            .unwrap()
            .unwrap()
            .as_ref(),
        &[10u8]
    );
    assert_eq!(
        manager
            .get(account(1, Space::Ethereum))
            .unwrap()
            .unwrap()
            .as_ref(),
        &[11u8]
    );

    let root = manager.commit().unwrap();
    assert_ne!(root.delta_root, MERKLE_NULL_NODE);
}

#[test]
fn file_store_recovers_latest_only() {
    let path = std::env::temp_dir().join(format!(
        "cfx-minimal-mpt-api-test-{}.bin",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);

    {
        let mut manager = StateManager::new(FileStore::new(&path)).unwrap();
        manager
            .set(account(5, Space::Native), Box::from([1u8, 2, 3]))
            .unwrap();
        manager.commit().unwrap();
    }

    let manager = StateManager::new(FileStore::new(&path)).unwrap();
    assert_eq!(
        manager
            .get(account(5, Space::Native))
            .unwrap()
            .unwrap()
            .as_ref(),
        &[1u8, 2, 3]
    );
    let _ = fs::remove_file(&path);
}

#[test]
fn all_key_shapes_roundtrip_in_both_spaces() {
    let mut manager = StateManager::new(MemoryStore::new()).unwrap();
    let mut expected = Vec::new();

    for space in [Space::Native, Space::Ethereum] {
        for (idx, key) in all_key_shapes(9, space).into_iter().enumerate() {
            let value = vec![idx as u8, if space == Space::Native { 1 } else { 2 }];
            manager
                .set(key.clone(), value.clone().into_boxed_slice())
                .unwrap();
            expected.push((key, value));
        }
    }

    for (key, value) in expected {
        assert_eq!(
            manager.get(key).unwrap().unwrap().as_ref(),
            value.as_slice()
        );
    }
}

#[test]
fn key_codec_roundtrips_all_shapes_and_short_test_keys() {
    for space in [Space::Native, Space::Ethereum] {
        for key in all_key_shapes(12, space) {
            let snapshot = key.to_key_bytes().unwrap();
            assert_eq!(StorageKeyWithSpace::from_key_bytes(&snapshot).unwrap(), key);

            let delta = key
                .to_delta_mpt_key_bytes(&DeltaMptKeyPadding::genesis())
                .unwrap();
            assert_eq!(
                StorageKeyWithSpace::from_delta_mpt_key(&delta).unwrap(),
                key
            );
        }
    }

    let short = StorageKeyWithSpace {
        key: StorageKey::Account(vec![1, 2, 3]),
        space: Space::Native,
    };
    assert_eq!(short.to_key_bytes().unwrap(), vec![1, 2, 3]);
    assert_eq!(
        short
            .to_delta_mpt_key_bytes(&DeltaMptKeyPadding::genesis())
            .unwrap(),
        vec![1, 2, 3]
    );
    assert_eq!(
        StorageKeyWithSpace::from_delta_mpt_key(&[1, 2, 3]).unwrap(),
        short
    );
}

#[test]
fn key_codec_rejects_malformed_keys() {
    let bad_address = StorageKeyWithSpace {
        key: StorageKey::StorageRoot(vec![1, 2, 3]),
        space: Space::Native,
    };
    assert!(bad_address.to_key_bytes().is_err());
    assert!(bad_address
        .to_delta_mpt_key_bytes(&DeltaMptKeyPadding::genesis())
        .is_err());

    let mut bad_snapshot = vec![7; 20];
    bad_snapshot.extend_from_slice(b"bad");
    assert!(StorageKeyWithSpace::from_key_bytes(&bad_snapshot).is_err());

    let mut bad_delta_marker = vec![0; 33];
    bad_delta_marker[32] = 0x82;
    assert!(StorageKeyWithSpace::from_delta_mpt_key(&bad_delta_marker).is_err());

    let mut short_storage_delta = vec![0; 32];
    short_storage_delta.extend_from_slice(b"datax");
    assert!(StorageKeyWithSpace::from_delta_mpt_key(&short_storage_delta).is_err());

    let mut unknown_delta_suffix = vec![0; 32];
    unknown_delta_suffix.extend_from_slice(b"bad");
    assert!(StorageKeyWithSpace::from_delta_mpt_key(&unknown_delta_suffix).is_err());
}

#[test]
fn types_format_errors_and_roots() {
    assert_eq!(H256::zero().as_bytes(), &[0u8; 32]);
    assert!(format!("{:?}", MERKLE_NULL_NODE).starts_with("0x"));

    let root = cfx_minimal_mpt::CommitRoot::new(
        MERKLE_NULL_NODE,
        MERKLE_NULL_NODE,
        MERKLE_NULL_NODE,
        DeltaMptKeyPadding::genesis().0,
    );
    assert_ne!(root.state_root_hash, H256::zero());

    let invalid = StorageKeyWithSpace {
        key: StorageKey::StorageRoot(vec![1]),
        space: Space::Native,
    }
    .to_key_bytes()
    .unwrap_err();
    assert_eq!(invalid.to_string(), "invalid key: address must be 20 bytes");
}

#[test]
fn file_store_reports_corrupt_state() {
    let path = std::env::temp_dir().join(format!(
        "cfx-minimal-mpt-corrupt-test-{}.bin",
        std::process::id()
    ));
    fs::write(&path, b"not bincode").unwrap();
    assert!(StateManager::new(FileStore::new(&path)).is_err());
    let _ = fs::remove_file(&path);
}

#[test]
fn empty_prefix_exposes_delta_values_because_bug_prefix_is_empty() {
    let mut manager = StateManager::new(MemoryStore::new()).unwrap();
    manager
        .set(account(1, Space::Native), Box::from([1u8]))
        .unwrap();
    manager
        .set(account(2, Space::Ethereum), Box::from([2u8]))
        .unwrap();

    let values = manager
        .get_all_by_prefix(StorageKeyWithSpace {
            key: StorageKey::Empty,
            space: Space::Native,
        })
        .unwrap()
        .unwrap();
    assert_eq!(values.len(), 2);
}

#[test]
fn storage_prefix_delete_matches_snapshot_but_misses_delta_storage_keys() {
    let prefix = vec![0xab, 0xcd];
    let full_key = [prefix.as_slice(), &[0x11; 30]].concat();
    let storage_key = storage(7, full_key.clone(), Space::Native);
    let prefix_key = storage(7, prefix, Space::Native);

    let mut snapshot = std::collections::BTreeMap::new();
    snapshot.insert(
        storage_key.to_key_bytes().unwrap(),
        Box::from([1u8]) as Box<[u8]>,
    );
    let mut state = cfx_minimal_mpt::State::from_snapshot(snapshot);
    let deleted = state
        .delete_all_by_prefix(prefix_key.clone())
        .unwrap()
        .unwrap();
    assert_eq!(deleted.len(), 1);
    assert!(state.get(storage_key.clone()).unwrap().is_none());

    let mut manager = StateManager::new(MemoryStore::new()).unwrap();
    manager.set(storage_key.clone(), Box::from([2u8])).unwrap();
    assert!(manager
        .get_all_by_prefix(prefix_key.clone())
        .unwrap()
        .is_none());
    assert!(manager.delete_all_by_prefix(prefix_key).unwrap().is_none());
    assert_eq!(manager.get(storage_key).unwrap().unwrap().as_ref(), &[2u8]);
}

#[test]
fn set_order_does_not_change_committed_root() {
    let keys: Vec<_> = (0u8..32).map(|id| account(id, Space::Native)).collect();
    let mut forward = StateManager::new(MemoryStore::new()).unwrap();
    let mut reverse = StateManager::new(MemoryStore::new()).unwrap();

    for (idx, key) in keys.iter().cloned().enumerate() {
        forward
            .set(key, vec![idx as u8, (idx * 7) as u8].into_boxed_slice())
            .unwrap();
    }
    for (idx, key) in keys.iter().cloned().enumerate().rev() {
        reverse
            .set(key, vec![idx as u8, (idx * 7) as u8].into_boxed_slice())
            .unwrap();
    }

    assert_eq!(forward.commit().unwrap(), reverse.commit().unwrap());
}

#[test]
fn layered_precedence_matches_delta_intermediate_snapshot_order() {
    let key = account(0x77, Space::Native);
    let raw = key
        .to_delta_mpt_key_bytes(&DeltaMptKeyPadding::genesis())
        .unwrap();
    let canonical = key.to_key_bytes().unwrap();
    let mut persisted = PersistedState::default();
    persisted.intermediate_mpt_key_padding = DeltaMptKeyPadding::genesis().0;
    persisted.delta_mpt_key_padding = DeltaMptKeyPadding::genesis().0;
    persisted
        .snapshot
        .insert(canonical.clone(), Box::from([1u8]) as Box<[u8]>);
    persisted
        .intermediate
        .insert(raw.clone(), MptValueDisk::Some(Box::from([2u8])));

    let mut state = cfx_minimal_mpt::State::from_persisted(persisted);
    assert_eq!(state.get(key.clone()).unwrap().unwrap().as_ref(), &[2u8]);

    state.set(key.clone(), Box::from([3u8])).unwrap();
    assert_eq!(state.get(key.clone()).unwrap().unwrap().as_ref(), &[3u8]);

    state.set(key.clone(), Box::new([])).unwrap();
    assert!(state.get(key).unwrap().is_none());
}

#[test]
fn commit_rolls_delta_to_intermediate_then_snapshot() {
    let key_a = account(0x21, Space::Native);
    let key_b = account(0x22, Space::Native);
    let mut manager = StateManager::with_snapshot_epoch_count(MemoryStore::new(), 2).unwrap();

    manager.set(key_a.clone(), Box::from([0xa1])).unwrap();
    let root_1 = manager.commit().unwrap();
    assert_eq!(root_1.snapshot_root, MERKLE_NULL_NODE);
    assert_eq!(root_1.intermediate_delta_root, MERKLE_NULL_NODE);
    assert_ne!(root_1.delta_root, MERKLE_NULL_NODE);

    let root_2 = manager.commit().unwrap();
    assert_eq!(root_2.snapshot_root, MERKLE_NULL_NODE);
    assert_eq!(root_2.intermediate_delta_root, MERKLE_NULL_NODE);
    assert_eq!(root_2.delta_root, root_1.delta_root);
    assert_eq!(manager.state().height(), 2);

    let root_3 = manager.commit().unwrap();
    assert_eq!(root_3.snapshot_root, MERKLE_NULL_NODE);
    assert_eq!(root_3.intermediate_delta_root, root_1.delta_root);
    assert_eq!(root_3.delta_root, MERKLE_NULL_NODE);
    assert_eq!(
        manager.get(key_a.clone()).unwrap().unwrap().as_ref(),
        &[0xa1]
    );

    manager.set(key_b.clone(), Box::from([0xb2])).unwrap();
    let root_4 = manager.commit().unwrap();
    assert_eq!(root_4.snapshot_root, MERKLE_NULL_NODE);
    assert_eq!(root_4.intermediate_delta_root, root_1.delta_root);
    assert_ne!(root_4.delta_root, MERKLE_NULL_NODE);

    let root_5 = manager.commit().unwrap();
    assert_ne!(root_5.snapshot_root, MERKLE_NULL_NODE);
    assert_eq!(root_5.intermediate_delta_root, root_4.delta_root);
    assert_eq!(root_5.delta_root, MERKLE_NULL_NODE);
    assert_eq!(
        manager.get(key_a.clone()).unwrap().unwrap().as_ref(),
        &[0xa1]
    );
    assert_eq!(
        manager.get(key_b.clone()).unwrap().unwrap().as_ref(),
        &[0xb2]
    );

    manager.set(key_a.clone(), Box::new([])).unwrap();
    let root_6 = manager.commit().unwrap();
    assert_ne!(root_6.delta_root, MERKLE_NULL_NODE);
    let root_7 = manager.commit().unwrap();
    assert!(manager.get(key_a.clone()).unwrap().is_none());
    assert_eq!(
        manager.get(key_b.clone()).unwrap().unwrap().as_ref(),
        &[0xb2]
    );
    assert_eq!(root_7.intermediate_delta_root, root_6.delta_root);

    let root_8 = manager.commit().unwrap();
    let root_9 = manager.commit().unwrap();
    assert_ne!(root_9.snapshot_root, root_8.snapshot_root);
    assert!(manager.get(key_a).unwrap().is_none());
    assert_eq!(manager.get(key_b).unwrap().unwrap().as_ref(), &[0xb2]);
}
