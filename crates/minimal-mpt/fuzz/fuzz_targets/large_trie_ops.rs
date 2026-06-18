#![no_main]

use cfx_minimal_mpt::{Space, State, StateTrait, StorageKey, StorageKeyWithSpace};
use libfuzzer_sys::fuzz_target;

const LARGE_UNIQUE_KEYS: usize = 5000;

fuzz_target!(|data: &[u8]| {
    let mut state = State::new();
    let base = data.first().copied().unwrap_or_default();
    let space = if data.get(1).copied().unwrap_or_default() & 1 == 0 {
        Space::Native
    } else {
        Space::Ethereum
    };

    for i in 0..LARGE_UNIQUE_KEYS {
        let key = storage_key(base, i, space);
        let value = [
            base,
            i as u8,
            (i >> 8) as u8,
            data.get(2 + i % data.len().max(1)).copied().unwrap_or(0),
        ];
        state.set(key, Box::from(value)).unwrap();
    }

    for i in [
        0,
        LARGE_UNIQUE_KEYS / 3,
        LARGE_UNIQUE_KEYS / 2,
        LARGE_UNIQUE_KEYS - 1,
    ] {
        assert!(state.get(storage_key(base, i, space)).unwrap().is_some());
    }

    let root_a = state.commit().unwrap();
    let root_b = state.commit().unwrap();
    assert_eq!(root_a, root_b);
});

fn storage_key(base: u8, index: usize, space: Space) -> StorageKeyWithSpace {
    let address = vec![base; 20];
    let mut storage_key = vec![0u8; 32];
    storage_key[24..].copy_from_slice(&(index as u64).to_be_bytes());
    StorageKeyWithSpace {
        key: StorageKey::Storage {
            address,
            storage_key,
        },
        space,
    }
}
