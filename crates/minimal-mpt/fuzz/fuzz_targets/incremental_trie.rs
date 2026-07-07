#![no_main]
//! Differential fuzz: for any sequence of set/delete ops, the incremental root
//! must equal the stateless `trie_root` oracle after every commit — computed the
//! same way regardless of which store backs the engine. We drive the SAME op
//! sequence through both production layer wrappers — `SnapshotTrie`
//! (two-level store, `SkipSingleton` policy) and `DeltaTrie` (BTree store,
//! `Full` policy) — and assert both agree with the oracle (hence with each
//! other), proving the engine is store- and policy-independent.
//!
//! Values are always live bytes; a delete reaches the snapshot as
//! `MptValue::Tombstone` (applied as a removal — its production contract) and
//! the delta as a plain `remove`. An empty live value hashes exactly like a
//! stored tombstone (`merkle_value` of both is `&[]`), so that hash path stays
//! covered; the stored-tombstone variant is delta-only and covered by the BTree
//! store's unit tests.

use cfx_minimal_mpt::{trie_root, DeltaTrie, MptValue, SnapshotTrie};
use libfuzzer_sys::fuzz_target;
use std::collections::BTreeMap;

fn value(byte: u8) -> Vec<u8> {
    // Empty values exercise the same hash path as stored tombstones.
    if byte % 5 == 0 {
        Vec::new()
    } else {
        vec![byte]
    }
}

fuzz_target!(|data: &[u8]| {
    let mut map: BTreeMap<Vec<u8>, MptValue> = BTreeMap::new();
    let mut snap = SnapshotTrie::default();
    let mut delta = DeltaTrie::default();

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
            snap.insert(&key, MptValue::Tombstone);
            delta.remove(&key);
        } else {
            let v = value(op);
            snap.insert(&key, MptValue::Some(v.clone().into_boxed_slice()));
            delta.insert(&key, MptValue::Some(v.clone().into_boxed_slice()));
            map.insert(key.clone(), MptValue::Some(v.into_boxed_slice()));
        }

        // Compare on roughly half the ops so the rest accumulate into batched
        // multi-dirty commits. Both layers must match the oracle (hence match
        // each other).
        if op % 2 == 0 {
            let oracle = trie_root(&map);
            assert_eq!(snap.root(), oracle);
            assert_eq!(delta.root(), oracle);
        }
    }

    let oracle = trie_root(&map);
    assert_eq!(snap.root(), oracle);
    assert_eq!(delta.root(), oracle);
});
