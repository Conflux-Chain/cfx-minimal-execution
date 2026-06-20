use cfx_minimal_mpt::{
    CommitRoot, DeltaMptKeyPadding, MemoryStore, Space, State, StateManager, StateTrait,
    StorageKey, StorageKeyWithSpace, H256,
};

fn main() {
    let mut state = State::new();

    for step in 0..800u32 {
        let id = trace_id(step);
        let key = account_key(id);
        match step % 11 {
            0 => {
                state.set(key, Box::new([])).unwrap();
                println!("D {step} {id}");
            }
            1 | 2 | 3 | 4 | 5 | 6 | 7 => {
                let value = value_for(step, id);
                state.set(key, value.clone().into_boxed_slice()).unwrap();
                println!("S {step} {id} {}", hex(&value));
            }
            8 | 9 => {
                let value = state.get(key).unwrap();
                println!(
                    "G {step} {id} {}",
                    value.map(|v| hex(&v)).unwrap_or_else(|| "-".to_string())
                );
            }
            _ => {
                let root = state.commit().unwrap();
                if step == 10 && std::env::var("CFX_TRACE_DUMP_KEYS").is_ok() {
                    dump_step_10_raw_keys();
                }
                println!(
                    "C {step} {} {}",
                    hex(root.delta_root.as_bytes()),
                    hex(root.state_root_hash.as_bytes())
                );
            }
        }
    }

    let root = state.commit().unwrap();
    println!(
        "F {} {}",
        hex(root.delta_root.as_bytes()),
        hex(root.state_root_hash.as_bytes())
    );

    trace_storage_prefix_delta_bug();
    trace_storage_prefix_snapshot_hit();
    trace_set_order();
    trace_short_account_prefix_delete_all();
    trace_set_delete();
    trace_snapshot_rollover();
    trace_intermediate_prefix_bug();
    trace_intermediate_account_prefix();
    trace_address_prefix_filter();
    trace_intermediate_address_prefix_filter();
}

fn trace_storage_prefix_delta_bug() {
    let mut state = State::new();
    let prefix = vec![0xab, 0xcd];
    let full_storage_key = [prefix.as_slice(), &[0x11; 30]].concat();
    let key = storage_key(7, full_storage_key, Space::Native);
    let prefix_key = storage_key(7, prefix, Space::Native);
    let value = vec![0x42, 0x24];

    state
        .set(key.clone(), value.clone().into_boxed_slice())
        .unwrap();
    println!("PSET {}", hex(&value));

    let read = state.get_all_by_prefix(prefix_key.clone()).unwrap();
    println!("PGET {}", format_prefix_result(read));

    let deleted = state.delete_all_by_prefix(prefix_key).unwrap();
    println!("PDEL {}", format_prefix_result(deleted));

    let after = state.get(key).unwrap();
    println!(
        "PPOST {}",
        after.map(|v| hex(&v)).unwrap_or_else(|| "-".to_string())
    );
}

fn trace_storage_prefix_snapshot_hit() {
    let prefix = vec![0xab, 0xcd];
    let full_storage_key = [prefix.as_slice(), &[0x22; 30]].concat();
    let key = storage_key(8, full_storage_key, Space::Native);
    let prefix_key = storage_key(8, prefix, Space::Native);
    let value = vec![0x55, 0x66];

    let mut snapshot = std::collections::BTreeMap::new();
    snapshot.insert(
        key.to_key_bytes().unwrap(),
        value.clone().into_boxed_slice(),
    );
    let mut state = State::from_snapshot(snapshot);

    let read = state.get_all_by_prefix(prefix_key.clone()).unwrap();
    println!("SNPGET {}", format_prefix_result(read));

    let deleted = state.delete_all_by_prefix(prefix_key).unwrap();
    println!("SNPDEL {}", format_prefix_result(deleted));

    let after = state.get(key).unwrap();
    println!(
        "SNPPOST {}",
        after.map(|v| hex(&v)).unwrap_or_else(|| "-".to_string())
    );
}

fn trace_set_order() {
    let mut forward = State::new();
    let mut reverse = State::new();
    let keys: Vec<_> = (0u8..24).map(account_key).collect();

    for (idx, key) in keys.iter().cloned().enumerate() {
        forward
            .set(
                key,
                vec![idx as u8, idx.wrapping_mul(3) as u8].into_boxed_slice(),
            )
            .unwrap();
    }
    for (idx, key) in keys.iter().cloned().enumerate().rev() {
        reverse
            .set(
                key,
                vec![idx as u8, idx.wrapping_mul(3) as u8].into_boxed_slice(),
            )
            .unwrap();
    }

    let a = forward.commit().unwrap();
    let b = reverse.commit().unwrap();
    println!(
        "ORDER {} {} {}",
        hex(a.delta_root.as_bytes()),
        hex(b.delta_root.as_bytes()),
        a.delta_root == b.delta_root
    );
}

fn trace_short_account_prefix_delete_all() {
    let mut state = State::new();
    for id in 0u8..8 {
        state
            .set(
                short_account(&[0x31, id]),
                vec![id, id + 1].into_boxed_slice(),
            )
            .unwrap();
    }
    for id in 0u8..5 {
        state
            .set(short_account(&[0x42, id]), vec![id + 10].into_boxed_slice())
            .unwrap();
    }

    let deleted = state.delete_all_by_prefix(short_account(&[0x31])).unwrap();
    println!("APDEL {}", format_prefix_result(deleted));

    let deleted_again = state.delete_all_by_prefix(short_account(&[0x31])).unwrap();
    println!("APDEL2 {}", format_prefix_result(deleted_again));

    let kept = state.get(short_account(&[0x42, 3])).unwrap();
    println!(
        "APKEEP {}",
        kept.map(|v| hex(&v)).unwrap_or_else(|| "-".to_string())
    );
}

fn trace_set_delete() {
    let mut state = State::new();
    let key = account_key(0x5a);
    state.set(key.clone(), Box::from([0x99u8])).unwrap();
    let before = state.get(key.clone()).unwrap();
    state.set(key.clone(), Box::new([])).unwrap();
    let after = state.get(key).unwrap();
    let root = state.commit().unwrap();
    println!(
        "SETDEL {} {} {}",
        before.map(|v| hex(&v)).unwrap_or_else(|| "-".to_string()),
        after.map(|v| hex(&v)).unwrap_or_else(|| "-".to_string()),
        hex(root.delta_root.as_bytes())
    );
}

fn trace_snapshot_rollover() {
    let key_a = account_key(0x21);
    let key_b = account_key(0x22);
    let key_c = account_key(0x23);
    let key_d = account_key(0x20);
    let mut state = StateManager::with_snapshot_epoch_count(MemoryStore::new(), 2).unwrap();

    state.set(key_a.clone(), Box::from([0xa1])).unwrap();
    println!("ROLL1 {}", format_root(&state.commit().unwrap()));

    println!("ROLL2 {}", format_root(&state.commit().unwrap()));
    println!("ROLL3 {}", format_root(&state.commit().unwrap()));
    println!(
        "ROLL3GET {}",
        state
            .get(key_a.clone())
            .unwrap()
            .map(|v| hex(&v))
            .unwrap_or_else(|| "-".to_string())
    );

    state.set(key_b.clone(), Box::from([0xb2])).unwrap();
    println!("ROLL4 {}", format_root(&state.commit().unwrap()));
    println!("ROLL5 {}", format_root(&state.commit().unwrap()));
    println!(
        "ROLL5GET {} {}",
        state
            .get(key_a.clone())
            .unwrap()
            .map(|v| hex(&v))
            .unwrap_or_else(|| "-".to_string()),
        state
            .get(key_b.clone())
            .unwrap()
            .map(|v| hex(&v))
            .unwrap_or_else(|| "-".to_string())
    );

    state.set(key_a.clone(), Box::new([])).unwrap();
    state.set(key_c.clone(), Box::from([0xc3])).unwrap();
    state.set(key_d.clone(), Box::from([0xd4])).unwrap();
    println!("ROLL6 {}", format_root(&state.commit().unwrap()));
    println!("ROLL7 {}", format_root(&state.commit().unwrap()));
    println!(
        "ROLL7GET {} {} {} {}",
        state
            .get(key_a.clone())
            .unwrap()
            .map(|v| hex(&v))
            .unwrap_or_else(|| "-".to_string()),
        state
            .get(key_b.clone())
            .unwrap()
            .map(|v| hex(&v))
            .unwrap_or_else(|| "-".to_string()),
        state
            .get(key_c.clone())
            .unwrap()
            .map(|v| hex(&v))
            .unwrap_or_else(|| "-".to_string()),
        state
            .get(key_d.clone())
            .unwrap()
            .map(|v| hex(&v))
            .unwrap_or_else(|| "-".to_string())
    );
    println!("ROLL8 {}", format_root(&state.commit().unwrap()));
    println!("ROLL9 {}", format_root(&state.commit().unwrap()));
    println!(
        "ROLL9GET {} {} {} {}",
        state
            .get(key_a)
            .unwrap()
            .map(|v| hex(&v))
            .unwrap_or_else(|| "-".to_string()),
        state
            .get(key_b)
            .unwrap()
            .map(|v| hex(&v))
            .unwrap_or_else(|| "-".to_string()),
        state
            .get(key_c)
            .unwrap()
            .map(|v| hex(&v))
            .unwrap_or_else(|| "-".to_string()),
        state
            .get(key_d)
            .unwrap()
            .map(|v| hex(&v))
            .unwrap_or_else(|| "-".to_string())
    );
}

fn trace_intermediate_prefix_bug() {
    let prefix = vec![0xab, 0xcd];
    let full_storage_key = [prefix.as_slice(), &[0x33; 30]].concat();
    let key = storage_key(9, full_storage_key, Space::Native);
    let prefix_key = storage_key(9, prefix, Space::Native);
    let mut state = StateManager::with_snapshot_epoch_count(MemoryStore::new(), 2).unwrap();

    state.set(key.clone(), Box::from([0x77])).unwrap();
    state.commit().unwrap();
    state.commit().unwrap();

    println!(
        "IPGET {}",
        format_prefix_result(state.get_all_by_prefix(prefix_key.clone()).unwrap())
    );
    println!(
        "IPDEL {}",
        format_prefix_result(state.delete_all_by_prefix(prefix_key).unwrap())
    );
    println!(
        "IPPOST {}",
        state
            .get(key)
            .unwrap()
            .map(|v| hex(&v))
            .unwrap_or_else(|| "-".to_string())
    );
}

fn trace_intermediate_account_prefix() {
    let mut state = StateManager::with_snapshot_epoch_count(MemoryStore::new(), 2).unwrap();
    for id in 0u8..4 {
        state
            .set(short_account(&[0x61, id]), vec![id + 1].into_boxed_slice())
            .unwrap();
    }
    state
        .set(short_account(&[0x62, 0]), Box::from([0x99]))
        .unwrap();
    state.commit().unwrap();
    state.commit().unwrap();

    let prefix = short_account(&[0x61]);
    println!(
        "IAPGET {}",
        format_prefix_result(state.get_all_by_prefix(prefix.clone()).unwrap())
    );
    println!(
        "IAPDEL {}",
        format_prefix_result(state.delete_all_by_prefix(prefix).unwrap())
    );
    println!(
        "IAPPOST {} {}",
        state
            .get(short_account(&[0x61, 2]))
            .unwrap()
            .map(|v| hex(&v))
            .unwrap_or_else(|| "-".to_string()),
        state
            .get(short_account(&[0x62, 0]))
            .unwrap()
            .map(|v| hex(&v))
            .unwrap_or_else(|| "-".to_string())
    );
}

fn trace_address_prefix_filter() {
    let mut state = State::new();
    let keep = short_account(&[0x52, 0x01]);
    let delete = short_account(&[0x51, 0x01]);
    let prefix = StorageKeyWithSpace {
        key: StorageKey::AddressPrefix(vec![0x51]),
        space: Space::Native,
    };

    state.set(keep.clone(), Box::from([0x10])).unwrap();
    state.set(delete.clone(), Box::from([0x20])).unwrap();
    println!(
        "ADDRDEL {}",
        format_prefix_result(state.delete_all_by_prefix(prefix).unwrap())
    );
    println!(
        "ADDRPOST {} {}",
        state
            .get(keep)
            .unwrap()
            .map(|v| hex(&v))
            .unwrap_or_else(|| "-".to_string()),
        state
            .get(delete)
            .unwrap()
            .map(|v| hex(&v))
            .unwrap_or_else(|| "-".to_string())
    );
}

fn trace_intermediate_address_prefix_filter() {
    let mut state = StateManager::with_snapshot_epoch_count(MemoryStore::new(), 2).unwrap();
    let keep = short_account(&[0x52, 0x02]);
    let delete = short_account(&[0x51, 0x02]);
    let prefix = StorageKeyWithSpace {
        key: StorageKey::AddressPrefix(vec![0x51]),
        space: Space::Native,
    };

    state.set(keep.clone(), Box::from([0x30])).unwrap();
    state.set(delete.clone(), Box::from([0x40])).unwrap();
    state.commit().unwrap();
    state.commit().unwrap();

    println!(
        "IADDRDEL {}",
        format_prefix_result(state.delete_all_by_prefix(prefix).unwrap())
    );
    println!(
        "IADDRPOST {} {}",
        state
            .get(keep)
            .unwrap()
            .map(|v| hex(&v))
            .unwrap_or_else(|| "-".to_string()),
        state
            .get(delete)
            .unwrap()
            .map(|v| hex(&v))
            .unwrap_or_else(|| "-".to_string())
    );
}

fn trace_id(step: u32) -> u8 {
    ((step.wrapping_mul(37).wrapping_add(11)) % 64) as u8
}

fn account_key(id: u8) -> StorageKeyWithSpace {
    StorageKeyWithSpace {
        key: StorageKey::Account(vec![id; 20]),
        space: if id % 3 == 0 {
            Space::Ethereum
        } else {
            Space::Native
        },
    }
}

fn short_account(bytes: &[u8]) -> StorageKeyWithSpace {
    StorageKeyWithSpace {
        key: StorageKey::Account(bytes.to_vec()),
        space: Space::Native,
    }
}

fn storage_key(address_byte: u8, storage_key: Vec<u8>, space: Space) -> StorageKeyWithSpace {
    StorageKeyWithSpace {
        key: StorageKey::Storage {
            address: vec![address_byte; 20],
            storage_key,
        },
        space,
    }
}

fn value_for(step: u32, id: u8) -> Vec<u8> {
    let len = 1 + (step as usize % 47);
    (0..len)
        .map(|i| id.wrapping_add(step as u8).wrapping_add(i as u8))
        .collect()
}

fn dump_step_10_raw_keys() {
    for step in 1..=7u32 {
        let id = trace_id(step);
        let key = account_key(id);
        let raw = key
            .to_delta_mpt_key_bytes(&DeltaMptKeyPadding::genesis(), None)
            .unwrap();
        eprintln!("RAW {step} {id} {}", hex(&raw));
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

fn format_prefix_result(values: Option<Vec<(Vec<u8>, Box<[u8]>)>>) -> String {
    let Some(mut values) = values else {
        return "-".to_string();
    };
    values.sort();
    values
        .into_iter()
        .map(|(key, value)| format!("{}={}", hex(&key), hex(&value)))
        .collect::<Vec<_>>()
        .join(",")
}

fn format_root(root: &CommitRoot) -> String {
    format!(
        "{}:{}:{}:{}",
        hex(root.snapshot_root.as_bytes()),
        hex(root.intermediate_delta_root.as_bytes()),
        hex(root.delta_root.as_bytes()),
        hex(root.state_root_hash.as_bytes())
    )
}

#[allow(dead_code)]
fn _assert_h256(_: H256) {}
