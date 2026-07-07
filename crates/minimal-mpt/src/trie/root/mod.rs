//! Incremental delta-trie root.
//!
//! The stateless [`crate::trie::trie_root`] rebuilds and re-hashes the entire
//! trie on every call. Inside a snapshot period the delta only grows, so doing
//! that every epoch is `O(N)` per commit and `O(N²)` per period — profiling
//! showed it dominating ~86% of replay CPU.
//!
//! The [`RootEngine`] keeps that exact hashing but skips the work for subtrees
//! that did not change:
//!
//! * Writers call [`RootEngine::mark_dirty`] with the changed canonical key —
//!   cheap, no hashing — and the recompute is **batched** to the next
//!   [`RootEngine::root`] call (never per-write).
//! * [`RootEngine::root`] mirrors `merkle_for_node` but, at each node, first asks
//!   "does any dirty key fall in this subtree's key range?". If not, and the
//!   subtree's hash is cached, it returns the cached hash **without recursing or
//!   hashing** — pruning the whole clean subtree in `O(log d)`.
//!
//! Only the hashing primitives are shared with [`crate::trie`]; the node merkle
//! rules are *not* duplicated, so the produced root is identical to `trie_root`
//! by construction. The structural skeleton (range partitioning, path
//! compression) is mirrored and guarded by a differential check against
//! `trie_root` in debug builds and the fuzz targets.
//!
//! ## Module layout
//!
//! * [`engine`] — [`RootEngine`]: the cache + dirty set + prefix eviction + root
//!   entry point, with the [`CachePolicy`] fixed at construction. It borrows the
//!   entries through a [`crate::trie::reader::TrieReader`] and never names `V`.
//! * [`memo`] — the hot serial [`memo::memo_node`] walk (cache-driven).
//! * [`rebuild`] — the cold parallel [`rebuild::memo_node_rebuild`] used to warm
//!   an empty cache over a large store.
//!
//! ## Key representation
//!
//! Keys are byte-aligned, so their **nibble length is even** and packing two
//! nibbles per byte reproduces the canonical byte key exactly. The entries store
//! is therefore keyed directly on canonical bytes — half the heap of the old
//! one-nibble-per-byte form — and nibble-level routing goes through the zero-cost
//! [`crate::nibbles::NibbleCursor`] view. Routing prefixes / cache keys can be odd
//! length, so they use [`crate::nibbles::PrefixKey`] (packed + a parity byte).
//!
//! ## Cache key and eviction
//!
//! A node is identified by its **routing nibble prefix** `P` (the `depth`
//! nibbles consumed to reach it): the subtree at `P` is exactly the keys whose
//! nibbles start with `P`, so `P` pins both its content and its position.
//!
//! Correctness reduces to one rule: **when key `K` is dirty, evict every
//! prefix of `K` from the cache.** A cached hash for `P` is stale iff the
//! subtree under `P` changed since it was cached, i.e. iff some dirty key `K`
//! has `P` as a prefix — and that commit evicts `P` (it is one of `K`'s
//! prefixes). The eviction happens at dirty time regardless of the current tree
//! shape, so it also covers a node that dissolved, had a descendant change
//! while gone, and later reappeared with the same prefix. A cache hit therefore
//! always means "content unchanged since cached" — no per-node dirty check is
//! needed during the walk.

mod engine;
mod memo;
mod rebuild;

pub(crate) use engine::{CachePolicy, RootEngine};

/// Entry-count threshold at/above which the cold cache rebuild fans out across
/// cores (and, in the serial walk, gates whether a full rebuild is used at all).
pub(crate) const PARALLEL_REBUILD_THRESHOLD: usize = 4096;
