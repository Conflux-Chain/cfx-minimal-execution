//! The storage interface the root/memo traversal requires.
//!
//! [`super::root`]'s incremental hash walk needs only a handful of read
//! operations from whatever store backs the trie: its size, an exact-key merkle
//! lookup, prefix existence / endpoints, and a full ordered iteration. Capturing
//! exactly those in [`TrieReader`] lets the traversal be generic over the store
//! and — crucially — makes the engine side see only **merkle bytes**, never the
//! stored value type `V`: every `V`-specific flattening (`TrieValue::merkle_value`)
//! happens inside the impl, behind the trait.
//!
//! Routing **prefixes** stay the packed `[parity ++ packed]` [`NibbleRef`] view
//! the walk builds on the stack (re-expressing them as [`Nibbles`] would force a
//! re-pack and defeat the store's zero-alloc range design); surfaced **keys**
//! come back as a borrowed [`Nibbles`] view. The sole implementor today is the
//! two-level store ([`super::store::two_level::Entries`]).

use crate::nibbles::{NibbleRef, Nibbles};

/// What the root/memo traversal needs from an entry store. Every method mirrors,
/// byte-for-byte in semantics, the concrete store method it abstracts — this is
/// an extraction, not a redefinition.
pub(crate) trait TrieReader {
    /// Number of stored entries (tombstones included).
    fn len(&self) -> usize;

    /// Whether the store holds no entries.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Merkle bytes of the value terminating **exactly** at `key`, or `None` if
    /// no key ends there (only an even / byte-aligned `key` can carry a value).
    /// A tombstone yields an empty slice — the stored value type never surfaces.
    fn get_merkle(&self, key: NibbleRef) -> Option<&[u8]>;

    /// Whether any stored key carries the nibble `prefix`.
    fn prefix_exists(&self, prefix: NibbleRef) -> bool;

    /// First (smallest) stored key carrying `prefix`, as a nibble view.
    fn prefix_first(&self, prefix: NibbleRef) -> Option<Nibbles<'_>>;

    /// Last (largest) stored key carrying `prefix`, as a nibble view.
    fn prefix_last(&self, prefix: NibbleRef) -> Option<Nibbles<'_>>;

    /// All entries in sorted key order as `(key nibbles, merkle bytes)`. The
    /// cold rebuild consumes this; the value type stays behind the trait.
    fn iter_merkle(&self) -> impl Iterator<Item = (Nibbles<'_>, &[u8])>;
}

/// The mutating side of an entry store, used by [`super::AuthenticatedTrie`] to
/// keep store and [`super::root::RootEngine`] in lock-step ("a write marks
/// dirty"). The stored value type is an associated type so the combinator can
/// forward writes without the *engine* ever naming `V` — only the combinator
/// (which owns both halves) does. Every method mirrors, byte-for-byte in
/// semantics, the concrete store method it abstracts. The sole implementor today
/// is the two-level store ([`super::store::two_level::Entries`]).
pub(crate) trait TrieStore: TrieReader {
    /// The stored value type.
    type Value;

    /// Insert / overwrite the value for `key` (the canonical byte mpt key).
    fn insert(&mut self, key: &[u8], value: Self::Value);

    /// Remove `key` (canonical byte form) from the store.
    fn remove(&mut self, key: &[u8]);

    /// Drop all entries.
    fn clear(&mut self);

    /// Feed an **ascending, sorted** batch (keys all greater than any already
    /// present) straight in — a dense build (see the two-level store).
    fn bulk_extend_sorted(&mut self, batch: Vec<(Vec<u8>, Self::Value)>);
}
