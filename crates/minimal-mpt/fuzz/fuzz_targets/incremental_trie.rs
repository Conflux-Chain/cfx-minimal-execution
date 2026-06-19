#![no_main]
//! Differential fuzz: the incremental delta-trie root must equal the stateless
//! `trie_root` oracle after every commit, for any sequence of set/delete ops.

use cfx_minimal_mpt::{trie_root, IncrementalTrie, MptValue};
use libfuzzer_sys::fuzz_target;
use std::collections::BTreeMap;

fn value(byte: u8) -> MptValue {
    // Empty value is a tombstone in the delta map.
    if byte % 5 == 0 {
        MptValue::Tombstone
    } else {
        MptValue::Some(Box::from([byte]))
    }
}

fuzz_target!(|data: &[u8]| {
    let mut map: BTreeMap<Vec<u8>, MptValue> = BTreeMap::new();
    let mut inc = IncrementalTrie::default();

    let mut i = 0usize;
    while i < data.len() {
        // Key length 0..=5 over a 6-symbol nibble alphabet, so keys share
        // prefixes and exercise path-compression splits/merges.
        let klen = (data[i] % 6) as usize;
        i += 1;
        if i + klen + 1 > data.len() {
            break;
        }
        let key: Vec<u8> = data[i..i + klen].iter().map(|b| b % 6).collect();
        i += klen;
        let op = data[i];
        i += 1;

        if op % 4 == 0 && map.contains_key(&key) {
            map.remove(&key);
            inc.remove(&key);
        } else {
            let v = value(op);
            inc.insert(&key, v.clone());
            map.insert(key.clone(), v);
        }

        // Compare on roughly half the ops so the rest accumulate into batched
        // multi-dirty commits.
        if op % 2 == 0 {
            assert_eq!(inc.root(), trie_root(&map));
        }
    }

    assert_eq!(inc.root(), trie_root(&map));
});
