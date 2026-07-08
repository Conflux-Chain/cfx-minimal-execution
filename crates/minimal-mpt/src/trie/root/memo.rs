//! Hot-path incremental node hashing.
//!
//! [`memo_node`] is the serial, cache-driven mirror of the stateless
//! `merkle_for_node`: it addresses each node's key block directly on the sorted
//! store via [`TrieReader`] prefix queries and short-circuits clean children on a
//! cache hit. Extracted verbatim from the old `incremental` module — the
//! algorithm (range partitioning, path compression, singleton skipping) is
//! unchanged.

use super::engine::{Cache, CachePolicy};
use crate::nibbles::{pack_nibbles, push_nibble, PrefixKey, PrefixRef};
use crate::primitives::{H256, MERKLE_NULL_NODE};
use crate::trie::reader::TrieReader;
use crate::trie::{compute_node_merkle, compute_path_merkle, CHILDREN};

/// Range-driven mirror of [`crate::trie`]'s `merkle_for_node`, plus a cache keyed
/// by the node's routing prefix `rp`. Instead of materialising the whole delta
/// into a slice and scanning it, the node's key block is addressed directly on
/// the sorted `store` via its [`TrieReader`] prefix queries, and clean children
/// short-circuit on a cache hit without ever touching the store. A hit is always
/// valid because `root` evicted any prefix touched by a dirty key (see module
/// docs). Generic over the store so the engine sees only merkle bytes, never `V`.
///
/// Cost is `O(visited nodes × CHILDREN × log N)` — proportional to the dirty
/// frontier, not the delta size. Serial only (the cache is `&mut`).
pub(super) fn memo_node<S: TrieReader>(
    store: &S,
    rp: PrefixRef,
    allow_path_compression: bool,
    cache: &mut Cache,
    cache_policy: CachePolicy,
) -> H256 {
    memo_node_via(
        store,
        rp,
        allow_path_compression,
        cache,
        cache_policy,
        &|s, child, c| memo_node(s, child, true, c, cache_policy),
    )
}

/// [`memo_node`] with the child recursion abstracted into `recurse`, so the
/// sharded cold rebuild ([`super::rebuild::memo_node_sharded`]) reuses this
/// entire node walk verbatim while diverting each child through its
/// materialize-below-cap escape. Generic over the closure — static dispatch,
/// same codegen as the direct self-recursion it replaces.
pub(super) fn memo_node_via<S, F>(
    store: &S,
    rp: PrefixRef,
    allow_path_compression: bool,
    cache: &mut Cache,
    cache_policy: CachePolicy,
    recurse: &F,
) -> H256
where
    S: TrieReader,
    F: Fn(&S, PrefixRef<'_>, &mut Cache) -> H256,
{
    if let Some(hash) = cache.get(rp.as_bytes()) {
        return *hash;
    }
    let depth = rp.nibble_len();

    // The node's key block is the contiguous key range for `rp`. It is non-empty:
    // the root is non-empty and this fn only descends into children proven to
    // exist. The first key carries the routing/compression nibbles; with path
    // compression the node absorbs the longest prefix shared by the whole block,
    // which — the block being sorted — is the common prefix of just its first
    // (min) and last (max) key.
    let first = store.prefix_first(rp).expect("non-empty node");
    let last = if allow_path_compression || cache_policy == CachePolicy::SkipSingleton {
        store.prefix_last(rp).expect("non-empty node")
    } else {
        first
    };
    // Value equality of the nibble path (not representation identity): a single
    // key spans the whole node iff its first and last key are the same sequence.
    let is_singleton = first.logical_eq(&last);
    let common = if allow_path_compression {
        first.common_prefix_len_from(&last, depth)
    } else {
        0
    };
    let node_depth = depth + common;
    // The node's full routing prefix, built once on the stack.
    let full_buf = pack_nibbles(first.sub(0, node_depth));
    let full = PrefixRef::new(&full_buf);

    // A key ending exactly at this node can only be `full` itself (and only when
    // it lands on a byte boundary — see `as_canonical_key`). The store flattens
    // any terminal value to its merkle bytes behind the trait.
    let maybe_value = store.get_merkle(full);

    // Resolve all 16 child slots left-to-right. Each child routing key is `full`
    // extended by one nibble into a fresh stack buffer (no heap): a cache hit
    // reuses the clean subtree's hash; on a miss, one range probe decides — a
    // present (dirty) child recurses, an absent one stays null.
    let children: [H256; CHILDREN] = std::array::from_fn(|c| {
        let mut child_buf = full_buf.clone();
        push_nibble(&mut child_buf, c as u8);
        if let Some(hash) = cache.get(&child_buf[..]) {
            return *hash;
        }
        let child = PrefixRef::new(&child_buf);
        if store.prefix_exists(child) {
            recurse(store, child, cache)
        } else {
            MERKLE_NULL_NODE
        }
    });

    let has_children = children.iter().any(|h| *h != MERKLE_NULL_NODE);
    let node_hash = compute_node_merkle(has_children.then_some(&children), maybe_value);
    let path = first.sub(depth, node_depth).to_expanded();
    let hash = compute_path_merkle(&path, depth, node_hash);

    if cache_policy == CachePolicy::Full || !is_singleton {
        cache.insert(PrefixKey::from_bytes(rp.as_bytes()), hash);
    }
    hash
}
