#![no_main]

use arbitrary::{Arbitrary, Unstructured};
use cfx_minimal_mpt::{
    Space, State, StateTrait, StorageKey, StorageKeyWithSpace,
};
use libfuzzer_sys::fuzz_target;
use std::collections::BTreeMap;

#[derive(Debug, Arbitrary)]
enum Op {
    Set { key: KeySeed, value: Vec<u8> },
    Delete { key: KeySeed },
    Get { key: KeySeed },
    PrefixGet { prefix: Vec<u8>, space: bool },
    PrefixDelete { prefix: Vec<u8>, space: bool },
    Commit,
}

#[derive(Clone, Debug, Arbitrary, PartialEq, Eq, PartialOrd, Ord)]
struct KeySeed {
    id: u8,
    kind: u8,
    space: bool,
    suffix: Vec<u8>,
}

fuzz_target!(|data: &[u8]| {
    let mut u = Unstructured::new(data);
    let ops = Vec::<Op>::arbitrary(&mut u).unwrap_or_default();
    let mut state = State::new();
    let mut model = BTreeMap::<Vec<u8>, Option<Vec<u8>>>::new();

    for op in ops.into_iter().take(2048) {
        match op {
            Op::Set { key, value } => {
                if let Ok(storage_key) = key.to_storage_key() {
                    let model_key = storage_key.to_key_bytes().unwrap();
                    let value = normalize_value(value);
                    state.set(storage_key, value.clone().into_boxed_slice()).unwrap();
                    model.insert(model_key, if value.is_empty() { None } else { Some(value) });
                }
            }
            Op::Delete { key } => {
                if let Ok(storage_key) = key.to_storage_key() {
                    let model_key = storage_key.to_key_bytes().unwrap();
                    state.set(storage_key, Box::new([])).unwrap();
                    model.insert(model_key, None);
                }
            }
            Op::Get { key } => {
                if let Ok(storage_key) = key.to_storage_key() {
                    let model_key = storage_key.to_key_bytes().unwrap();
                    let actual = state.get(storage_key).unwrap().map(|v| v.to_vec());
                    let expected = model.get(&model_key).cloned().flatten();
                    assert_eq!(actual, expected);
                }
            }
            Op::PrefixGet { prefix, space } => {
                let _ = state.get_all_by_prefix(prefix_key(prefix, space)).unwrap();
            }
            Op::PrefixDelete { prefix, space } => {
                let _ = state.delete_all_by_prefix(prefix_key(prefix, space)).unwrap();
                model.clear();
            }
            Op::Commit => {
                let a = state.commit().unwrap();
                let b = state.commit().unwrap();
                assert_eq!(a, b);
            }
        }
    }
});

impl KeySeed {
    fn to_storage_key(&self) -> Result<StorageKeyWithSpace, ()> {
        let address = vec![self.id; 20];
        let key = match self.kind % 7 {
            0 => StorageKey::Account(address),
            1 => StorageKey::StorageRoot(address),
            2 => StorageKey::Storage {
                address,
                storage_key: normalize_suffix(&self.suffix, 32),
            },
            3 => StorageKey::CodeRoot(address),
            4 => StorageKey::Code {
                address,
                code_hash: normalize_suffix(&self.suffix, 32),
            },
            5 => StorageKey::DepositList(address),
            _ => StorageKey::VoteList(address),
        };
        Ok(StorageKeyWithSpace {
            key,
            space: if self.space { Space::Ethereum } else { Space::Native },
        })
    }
}

fn prefix_key(mut prefix: Vec<u8>, space: bool) -> StorageKeyWithSpace {
    prefix.truncate(20);
    StorageKeyWithSpace {
        key: StorageKey::AddressPrefix(prefix),
        space: if space { Space::Ethereum } else { Space::Native },
    }
}

fn normalize_value(mut value: Vec<u8>) -> Vec<u8> {
    value.truncate(256);
    value
}

fn normalize_suffix(bytes: &[u8], len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    for (i, b) in bytes.iter().take(len).enumerate() {
        out[i] = *b;
    }
    out
}
