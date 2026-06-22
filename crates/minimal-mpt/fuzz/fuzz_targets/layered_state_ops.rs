#![no_main]

use arbitrary::{Arbitrary, Unstructured};
use cfx_minimal_mpt::{
    trie_root, DeltaMptKeyPadding, MptKeyValue, MptValue, MptValueDisk, PersistedState, Space,
    State, StateTrait, StorageKey, StorageKeyWithSpace,
};
use libfuzzer_sys::fuzz_target;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Arbitrary)]
struct Case {
    snapshot_epoch_count: u8,
    snapshot: Vec<Entry>,
    intermediate: Vec<Entry>,
    ops: Vec<Op>,
}

#[derive(Clone, Debug, Arbitrary)]
struct Entry {
    key: KeySeed,
    value: Vec<u8>,
    tombstone: bool,
}

#[derive(Debug, Arbitrary)]
enum Op {
    Set { key: KeySeed, value: Vec<u8> },
    Delete { key: KeySeed },
    Get { key: KeySeed },
    PrefixGet { prefix: PrefixSeed },
    PrefixDelete { prefix: PrefixSeed },
    Commit,
    PersistRoundtrip,
}

#[derive(Clone, Debug, Arbitrary, PartialEq, Eq, PartialOrd, Ord)]
struct KeySeed {
    id: u8,
    kind: u8,
    space: bool,
    suffix: Vec<u8>,
}

#[derive(Clone, Debug, Arbitrary)]
struct PrefixSeed {
    key: KeySeed,
    kind: u8,
    len: u8,
    bytes: Vec<u8>,
}

struct Model {
    snapshot: BTreeMap<Vec<u8>, Vec<u8>>,
    intermediate: BTreeMap<Vec<u8>, ModelValue>,
    delta: BTreeMap<Vec<u8>, ModelValue>,
    intermediate_padding: DeltaMptKeyPadding,
    delta_padding: DeltaMptKeyPadding,
    height: u64,
    snapshot_epoch_count: u32,
}

#[derive(Clone)]
struct ModelValue {
    raw_key: Vec<u8>,
    value: Option<Vec<u8>>,
}

fuzz_target!(|data: &[u8]| {
    let mut u = Unstructured::new(data);
    let case = Case::arbitrary(&mut u).unwrap_or(Case {
        snapshot_epoch_count: 0,
        snapshot: Vec::new(),
        intermediate: Vec::new(),
        ops: Vec::new(),
    });
    let snapshot_epoch_count = 1 + (case.snapshot_epoch_count as u32 % 8);

    let mut model = Model::new(snapshot_epoch_count);
    for entry in case.snapshot.into_iter().take(64) {
        if let Ok(key) = entry.key.to_storage_key() {
            if !entry.tombstone {
                model
                    .snapshot
                    .insert(key.to_key_bytes().unwrap(), normalize_value(entry.value));
            }
        }
    }

    let mut persisted = PersistedState {
        snapshot: model
            .snapshot
            .iter()
            .map(|(k, v)| (k.clone(), v.clone().into_boxed_slice()))
            .collect(),
        intermediate_mpt_key_padding: DeltaMptKeyPadding::genesis().0,
        delta_mpt_key_padding: DeltaMptKeyPadding::genesis().0,
        snapshot_epoch_count,
        ..PersistedState::default()
    };

    for entry in case.intermediate.into_iter().take(64) {
        if let Ok(key) = entry.key.to_storage_key() {
            let canonical = key.to_key_bytes().unwrap();
            let raw = key
                .to_delta_mpt_key_bytes(&DeltaMptKeyPadding::genesis(), None)
                .unwrap();
            let value = if entry.tombstone {
                ModelValue {
                    raw_key: raw,
                    value: None,
                }
            } else {
                ModelValue {
                    raw_key: raw,
                    value: Some(normalize_value(entry.value)),
                }
            };
            model.intermediate.insert(canonical, value.clone());
            persisted.intermediate.insert(
                value.raw_key,
                match value.value {
                    Some(value) => MptValueDisk::Some(value.into_boxed_slice()),
                    None => MptValueDisk::Tombstone,
                },
            );
        }
    }

    let mut state = State::from_persisted(persisted);

    for op in case.ops.into_iter().take(2048) {
        match op {
            Op::Set { key, value } => {
                if let Ok(key) = key.to_storage_key() {
                    let canonical = key.to_key_bytes().unwrap();
                    let raw_key = key
                        .to_delta_mpt_key_bytes(&model.delta_padding, None)
                        .unwrap();
                    let value = normalize_value(value);
                    state.set(key, value.clone().into_boxed_slice()).unwrap();
                    model.delta.insert(
                        canonical,
                        ModelValue {
                            raw_key,
                            value: if value.is_empty() { None } else { Some(value) },
                        },
                    );
                }
            }
            Op::Delete { key } => {
                if let Ok(key) = key.to_storage_key() {
                    let canonical = key.to_key_bytes().unwrap();
                    let raw_key = key
                        .to_delta_mpt_key_bytes(&model.delta_padding, None)
                        .unwrap();
                    state.set(key, Box::new([])).unwrap();
                    model.delta.insert(
                        canonical,
                        ModelValue {
                            raw_key,
                            value: None,
                        },
                    );
                }
            }
            Op::Get { key } => {
                if let Ok(key) = key.to_storage_key() {
                    let canonical = key.to_key_bytes().unwrap();
                    let actual = state.get(key).unwrap().map(|v| v.to_vec());
                    assert_eq!(actual, model.get(&canonical));
                }
            }
            Op::PrefixGet { prefix } => {
                let prefix = prefix.to_storage_key();
                let actual = normalize_kvs(state.get_all_by_prefix(prefix.clone()).unwrap());
                let expected = normalize_kvs(model.prefix_get(&prefix));
                assert_eq!(actual, expected);
            }
            Op::PrefixDelete { prefix } => {
                let prefix = prefix.to_storage_key();
                let actual = normalize_kvs(state.delete_all_by_prefix(prefix.clone()).unwrap());
                let expected = normalize_kvs(model.prefix_delete(&prefix));
                assert_eq!(actual, expected);
                assert_model_matches_state(&state, &model);
            }
            Op::Commit => {
                let root = state.commit().unwrap();
                model.advance_after_commit(root.delta_root);
                assert_model_matches_state(&state, &model);
            }
            Op::PersistRoundtrip => {
                let root = state.commit().unwrap();
                model.advance_after_commit(root.delta_root);
                state = State::from_persisted(state.persisted());
                assert_model_matches_state(&state, &model);
            }
        }
    }
});

impl Model {
    fn new(snapshot_epoch_count: u32) -> Self {
        Self {
            snapshot: BTreeMap::new(),
            intermediate: BTreeMap::new(),
            delta: BTreeMap::new(),
            intermediate_padding: DeltaMptKeyPadding::genesis(),
            delta_padding: DeltaMptKeyPadding::genesis(),
            height: 0,
            snapshot_epoch_count,
        }
    }

    fn advance_after_commit(&mut self, delta_root: cfx_minimal_mpt::H256) {
        self.height += 1;
        if self.height % self.snapshot_epoch_count as u64 != 0 {
            return;
        }

        for (canonical, value) in std::mem::take(&mut self.intermediate) {
            match value.value {
                Some(value) => {
                    self.snapshot.insert(canonical, value);
                }
                None => {
                    self.snapshot.remove(&canonical);
                }
            }
        }

        let snapshot_root = trie_root(
            &self
                .snapshot
                .iter()
                .map(|(k, v)| (k.clone(), MptValue::Some(v.clone().into_boxed_slice())))
                .collect(),
        );
        self.intermediate = std::mem::take(&mut self.delta);
        self.intermediate_padding = self.delta_padding.clone();
        self.delta_padding = DeltaMptKeyPadding::from_roots(snapshot_root, delta_root);
    }

    fn get(&self, canonical: &[u8]) -> Option<Vec<u8>> {
        if let Some(value) = self.delta.get(canonical) {
            return value.value.clone();
        }
        if let Some(value) = self.intermediate.get(canonical) {
            return value.value.clone();
        }
        self.snapshot.get(canonical).cloned()
    }

    fn prefix_get(&self, prefix: &StorageKeyWithSpace) -> Option<Vec<MptKeyValue>> {
        let canonical_prefix = prefix.to_key_bytes().unwrap();
        let delta_prefix = prefix
            .to_delta_mpt_key_bytes(&self.delta_padding, None)
            .unwrap();
        let intermediate_prefix = prefix
            .to_delta_mpt_key_bytes(&self.intermediate_padding, None)
            .unwrap();
        let address_prefix = address_prefix(prefix);
        let mut seen = BTreeSet::new();
        let mut out = Vec::new();

        for (key, value) in self.delta.iter() {
            if !value.raw_key.starts_with(&delta_prefix) {
                continue;
            }
            if address_prefix.is_some_and(|prefix| !key.starts_with(prefix)) {
                continue;
            }
            seen.insert(key.clone());
            if let Some(value) = &value.value {
                out.push((key.clone(), value.clone().into_boxed_slice()));
            }
        }
        for (key, value) in self.intermediate.iter() {
            if !value.raw_key.starts_with(&intermediate_prefix) {
                continue;
            }
            if address_prefix.is_some_and(|prefix| !key.starts_with(prefix)) {
                continue;
            }
            if seen.insert(key.clone()) {
                if let Some(value) = &value.value {
                    out.push((key.clone(), value.clone().into_boxed_slice()));
                }
            }
        }
        for (key, value) in self.snapshot.range(canonical_prefix.clone()..) {
            if !key.starts_with(&canonical_prefix) {
                break;
            }
            if seen.insert(key.clone()) {
                out.push((key.clone(), value.clone().into_boxed_slice()));
            }
        }

        (!out.is_empty()).then_some(out)
    }

    fn prefix_delete(&mut self, prefix: &StorageKeyWithSpace) -> Option<Vec<MptKeyValue>> {
        let canonical_prefix = prefix.to_key_bytes().unwrap();
        let delta_prefix = prefix
            .to_delta_mpt_key_bytes(&self.delta_padding, None)
            .unwrap();
        let intermediate_prefix = prefix
            .to_delta_mpt_key_bytes(&self.intermediate_padding, None)
            .unwrap();
        let address_prefix = address_prefix(prefix);
        let mut seen = BTreeSet::new();
        let mut out = Vec::new();

        let delta_keys: Vec<_> = self
            .delta
            .iter()
            .filter(|(_, value)| value.raw_key.starts_with(&delta_prefix))
            .map(|(key, _)| key.clone())
            .collect();
        for key in delta_keys {
            let value = self.delta.remove(&key).unwrap();
            if address_prefix.is_some_and(|prefix| !key.starts_with(prefix)) {
                continue;
            }
            seen.insert(key.clone());
            if let Some(value) = value.value {
                out.push((key, value.into_boxed_slice()));
            }
        }

        let intermediate_keys: Vec<_> = self
            .intermediate
            .iter()
            .filter(|(_, value)| value.raw_key.starts_with(&intermediate_prefix))
            .map(|(key, _)| key.clone())
            .collect();
        for key in intermediate_keys {
            let value = self.intermediate.get(&key).cloned().unwrap();
            if address_prefix.is_some_and(|prefix| !key.starts_with(prefix)) {
                continue;
            }
            if value.value.is_some() {
                let storage_key = StorageKeyWithSpace::from_key_bytes(&key).unwrap();
                self.delta.insert(
                    key.clone(),
                    ModelValue {
                        raw_key: storage_key
                            .to_delta_mpt_key_bytes(&self.delta_padding, None)
                            .unwrap(),
                        value: None,
                    },
                );
            }
            if seen.insert(key.clone()) {
                if let Some(value) = value.value {
                    out.push((key, value.into_boxed_slice()));
                }
            }
        }

        let snapshot_keys: Vec<_> = self
            .snapshot
            .range(canonical_prefix.clone()..)
            .take_while(|(key, _)| key.starts_with(&canonical_prefix))
            .map(|(key, _)| key.clone())
            .collect();
        for key in snapshot_keys {
            let value = self.snapshot.get(&key).cloned().unwrap();
            let storage_key = StorageKeyWithSpace::from_key_bytes(&key).unwrap();
            self.delta.insert(
                key.clone(),
                ModelValue {
                        raw_key: storage_key
                        .to_delta_mpt_key_bytes(&self.delta_padding, None)
                        .unwrap(),
                    value: None,
                },
            );
            if seen.insert(key.clone()) {
                out.push((key, value.into_boxed_slice()));
            }
        }

        (!out.is_empty()).then_some(out)
    }
}

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
            space: if self.space {
                Space::Ethereum
            } else {
                Space::Native
            },
        })
    }
}

impl PrefixSeed {
    fn to_storage_key(&self) -> StorageKeyWithSpace {
        if self.kind % 4 == 0 {
            let mut prefix = self.bytes.clone();
            if prefix.is_empty() {
                prefix.push(self.key.id);
            }
            prefix.truncate((self.len as usize) % 21);
            return StorageKeyWithSpace {
                key: StorageKey::AddressPrefix(prefix),
                space: if self.key.space {
                    Space::Ethereum
                } else {
                    Space::Native
                },
            };
        }
        if self.kind % 4 == 1 {
            let mut storage_prefix = normalize_suffix(&self.key.suffix, 32);
            storage_prefix.truncate((self.len as usize) % 33);
            return StorageKeyWithSpace {
                key: StorageKey::Storage {
                    address: vec![self.key.id; 20],
                    storage_key: storage_prefix,
                },
                space: if self.key.space {
                    Space::Ethereum
                } else {
                    Space::Native
                },
            };
        }
        self.key.to_storage_key().unwrap()
    }
}

fn assert_model_matches_state(state: &State, model: &Model) {
    let mut keys = BTreeSet::new();
    keys.extend(model.snapshot.keys().cloned());
    keys.extend(model.intermediate.keys().cloned());
    keys.extend(model.delta.keys().cloned());
    for canonical in keys {
        let storage_key = StorageKeyWithSpace::from_key_bytes(&canonical).unwrap();
        let actual = state.get(storage_key).unwrap().map(|v| v.to_vec());
        let expected = model.get(&canonical);
        assert_eq!(
            actual,
            expected,
            "canonical={canonical:?} delta={:?} intermediate={:?} snapshot={:?}",
            model.delta.get(&canonical).map(|v| &v.value),
            model.intermediate.get(&canonical).map(|v| &v.value),
            model.snapshot.get(&canonical),
        );
    }
}

fn address_prefix(prefix: &StorageKeyWithSpace) -> Option<&[u8]> {
    match &prefix.key {
        StorageKey::AddressPrefix(prefix) => Some(prefix),
        _ => None,
    }
}

fn normalize_kvs(values: Option<Vec<MptKeyValue>>) -> Option<Vec<(Vec<u8>, Vec<u8>)>> {
    values.map(|values| {
        let mut values: Vec<_> = values
            .into_iter()
            .map(|(key, value)| (key, value.to_vec()))
            .collect();
        values.sort();
        values
    })
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
