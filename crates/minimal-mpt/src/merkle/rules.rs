use crate::types::{H256, MERKLE_NULL_NODE};
use tiny_keccak::{Hasher, Keccak};

pub(crate) const CHILDREN: usize = 16;

pub fn keccak(data: &[u8]) -> H256 {
    let mut out = [0u8; 32];
    let mut hasher = Keccak::v256();
    hasher.update(data);
    hasher.finalize(&mut out);
    H256(out)
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
    path_nibbles: &[u8],
    start_depth: usize,
    node_merkle: H256,
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

fn compress_nibbles(nibbles: &[u8], without_first_nibble: bool) -> Vec<u8> {
    let (first, rest) = if without_first_nibble {
        let (first, rest) = nibbles.split_first().expect("non-empty path");
        (Some(*first), rest)
    } else {
        (None, nibbles)
    };

    let mut out = Vec::with_capacity(first.map_or(0, |_| 1) + rest.len().div_ceil(2));
    if let Some(first) = first {
        out.push(first);
    }
    for pair in rest.chunks(2) {
        let hi = pair[0] << 4;
        let lo = pair.get(1).copied().unwrap_or(0);
        out.push(hi | lo);
    }
    out
}
