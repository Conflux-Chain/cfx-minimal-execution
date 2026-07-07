use crate::merkle::rules::{compute_node_merkle, compute_path_merkle, CHILDREN};
use crate::primitives::{H256, MERKLE_NULL_NODE};
use crate::trie::MptValue;
use rayon::prelude::*;
use std::collections::BTreeMap;

const PARALLEL_HASH_THRESHOLD: usize = 4096;

pub fn trie_root(kv: &BTreeMap<Vec<u8>, MptValue>) -> H256 {
    if kv.is_empty() {
        return MERKLE_NULL_NODE;
    }
    let entries: Vec<(Vec<u8>, &MptValue)> =
        kv.iter().map(|(k, v)| (bytes_to_nibbles(k), v)).collect();
    root_for_entries(&entries)
}

pub(crate) fn root_for_entries(entries: &[(Vec<u8>, &MptValue)]) -> H256 {
    if entries.is_empty() {
        return MERKLE_NULL_NODE;
    }
    merkle_for_node(entries, 0, entries.len() >= PARALLEL_HASH_THRESHOLD, false)
}

fn merkle_for_node(
    entries: &[(Vec<u8>, &MptValue)],
    depth: usize,
    parallel: bool,
    allow_path_compression: bool,
) -> H256 {
    let common = if allow_path_compression {
        common_prefix_len(entries, depth)
    } else {
        0
    };
    let node_depth = depth + common;

    let maybe_value = entries
        .iter()
        .find(|(key, _)| key.len() == node_depth)
        .map(|(_, value)| value.merkle_value());

    let mut ranges: Vec<(usize, usize, usize)> = Vec::new();
    let mut iter = entries.iter().enumerate().peekable();
    while let Some((start, (key, _))) = iter.next() {
        if key.len() == node_depth {
            continue;
        }
        let child = key[node_depth] as usize;
        while iter
            .next_if(|(_, (key, _))| key.len() > node_depth && key[node_depth] as usize == child)
            .is_some()
        {}
        let end = iter.peek().map_or(entries.len(), |(idx, _)| *idx);
        ranges.push((child, start, end));
    }

    let merkle_for_child = |&(child, start, end): &(usize, usize, usize)| {
        (
            child,
            merkle_for_node(&entries[start..end], node_depth + 1, true, true),
        )
    };
    let child_hashes: Vec<_> = if parallel && entries.len() >= PARALLEL_HASH_THRESHOLD {
        ranges.par_iter().map(merkle_for_child).collect()
    } else {
        ranges.iter().map(merkle_for_child).collect()
    };

    let mut children: [H256; CHILDREN] = [MERKLE_NULL_NODE; CHILDREN];
    for (child, hash) in child_hashes {
        children[child] = hash;
    }

    let has_children = children.iter().any(|h| *h != MERKLE_NULL_NODE);
    let node_hash = compute_node_merkle(has_children.then_some(&children), maybe_value);
    compute_path_merkle(&entries[0].0[depth..node_depth], depth, node_hash)
}

fn common_prefix_len(entries: &[(Vec<u8>, &MptValue)], depth: usize) -> usize {
    // TwoLevelStore are sorted, so the common prefix equals that of the endpoints.
    match entries {
        [(first, _), .., (last, _)] => first
            .iter()
            .zip(last)
            .skip(depth)
            .take_while(|(a, b)| a == b)
            .count(),
        [(k, _)] => k.len().saturating_sub(depth),
        [] => 0,
    }
}

pub(crate) fn bytes_to_nibbles(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(b >> 4);
        out.push(b & 0x0f);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_root_is_null() {
        assert_eq!(trie_root(&BTreeMap::new()), MERKLE_NULL_NODE);
    }

    #[test]
    fn root_is_order_independent() {
        let mut a = BTreeMap::new();
        a.insert(vec![0x12], MptValue::Some(Box::from([1u8])));
        a.insert(vec![0x13], MptValue::Some(Box::from([2u8])));
        let root_a = trie_root(&a);

        let mut b = BTreeMap::new();
        b.insert(vec![0x13], MptValue::Some(Box::from([2u8])));
        b.insert(vec![0x12], MptValue::Some(Box::from([1u8])));
        assert_eq!(root_a, trie_root(&b));
    }
}
