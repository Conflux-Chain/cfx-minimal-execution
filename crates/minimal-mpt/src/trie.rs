use crate::types::{H256, MERKLE_NULL_NODE};
use rayon::prelude::*;
use std::collections::BTreeMap;
use tiny_keccak::{Hasher, Keccak};

pub(crate) const CHILDREN: usize = 16;
const PARALLEL_HASH_THRESHOLD: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MptValue {
    Some(Box<[u8]>),
    Tombstone,
}

impl MptValue {
    pub fn visible_value(&self) -> Option<&[u8]> {
        match self {
            Self::Some(v) => Some(v),
            Self::Tombstone => None,
        }
    }

    pub(crate) fn merkle_value(&self) -> &[u8] {
        match self {
            Self::Some(v) => v,
            Self::Tombstone => &[],
        }
    }

    pub fn is_tombstone(&self) -> bool {
        matches!(self, Self::Tombstone)
    }
}

pub fn keccak(data: &[u8]) -> H256 {
    let mut out = [0u8; 32];
    let mut hasher = Keccak::v256();
    hasher.update(data);
    hasher.finalize(&mut out);
    H256(out)
}

pub fn trie_root(kv: &BTreeMap<Vec<u8>, MptValue>) -> H256 {
    if kv.is_empty() {
        return MERKLE_NULL_NODE;
    }
    let entries: Vec<(Vec<u8>, &MptValue)> =
        kv.iter().map(|(k, v)| (bytes_to_nibbles(k), v)).collect();
    root_for_entries(&entries)
}

fn root_for_entries(entries: &[(Vec<u8>, &MptValue)]) -> H256 {
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
    let mut i = 0;
    while i < entries.len() {
        if entries[i].0.len() == node_depth {
            i += 1;
            continue;
        }
        let child = entries[i].0[node_depth] as usize;
        let start = i;
        i += 1;
        while i < entries.len()
            && entries[i].0.len() > node_depth
            && entries[i].0[node_depth] as usize == child
        {
            i += 1;
        }
        ranges.push((child, start, i));
    }

    let child_hashes: Vec<(usize, H256)> = if parallel && entries.len() >= PARALLEL_HASH_THRESHOLD {
        ranges
            .par_iter()
            .map(|(child, start, end)| {
                (
                    *child,
                    merkle_for_node(&entries[*start..*end], node_depth + 1, true, true),
                )
            })
            .collect()
    } else {
        ranges
            .iter()
            .map(|(child, start, end)| {
                (
                    *child,
                    merkle_for_node(&entries[*start..*end], node_depth + 1, false, true),
                )
            })
            .collect()
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
    if entries.is_empty() {
        return 0;
    }
    let first = &entries[0].0;
    let mut len = 0;
    'outer: loop {
        let idx = depth + len;
        if idx >= first.len() {
            break;
        }
        let nibble = first[idx];
        for (key, _) in entries.iter().skip(1) {
            if idx >= key.len() || key[idx] != nibble {
                break 'outer;
            }
        }
        len += 1;
    }
    len
}

pub(crate) fn compute_node_merkle(
    children_merkles: Option<&[H256; CHILDREN]>,
    maybe_value: Option<&[u8]>,
) -> H256 {
    let mut buffer = Vec::with_capacity(1 + 32 * CHILDREN + maybe_value.map_or(0, |v| 1 + v.len()));
    buffer.push(b'n');
    let empty = [MERKLE_NULL_NODE; CHILDREN];
    let children = children_merkles.unwrap_or(&empty);
    for child in children {
        buffer.extend_from_slice(child.as_bytes());
    }
    if let Some(value) = maybe_value {
        buffer.push(b'v');
        buffer.extend_from_slice(value);
    }
    keccak(&buffer)
}

pub(crate) fn compute_path_merkle(
    path_nibbles: &[u8], start_depth: usize, node_merkle: H256,
) -> H256 {
    if path_nibbles.is_empty() {
        return node_merkle;
    }
    let without_first_nibble = start_depth % 2 == 1;
    let compressed = compress_nibbles(path_nibbles, without_first_nibble);
    let path_info = 128u8 + 64u8 * (without_first_nibble as u8) + (path_nibbles.len() as u8 % 63);
    let mut buffer = Vec::with_capacity(1 + compressed.len() + 32);
    buffer.push(path_info);
    buffer.extend_from_slice(&compressed);
    buffer.extend_from_slice(node_merkle.as_bytes());
    keccak(&buffer)
}

pub(crate) fn bytes_to_nibbles(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(b >> 4);
        out.push(b & 0x0f);
    }
    out
}

fn compress_nibbles(nibbles: &[u8], without_first_nibble: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity((nibbles.len() + 1) / 2);
    let mut i = 0;
    if without_first_nibble {
        out.push(nibbles[0]);
        i = 1;
    }
    while i < nibbles.len() {
        let hi = nibbles[i] << 4;
        let lo = if i + 1 < nibbles.len() {
            nibbles[i + 1]
        } else {
            0
        };
        out.push(hi | lo);
        i += 2;
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
