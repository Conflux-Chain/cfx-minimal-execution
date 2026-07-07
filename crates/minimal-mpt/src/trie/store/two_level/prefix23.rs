//! Level-1 key ([`Prefix23`]), node payload ([`Node`]), and the split-key view
//! ([`Composite`]) of the two-level entry store. Split out of `two_level/mod.rs`;
//! all three stay two_level-internal (`pub(super)`), never named outside the store.

use super::{Level2Key, HEAD_LEN};
use crate::nibbles::Nibbles;
use std::collections::BTreeMap;

/// A level-1 key: up to [`HEAD_LEN`] key bytes plus their length. The length as
/// the last (least-significant) field makes `#[derive(Ord)]` reproduce
/// slice-lexicographic order (zero-padded data compares first, then a shorter
/// key — a prefix — sorts before its longer extensions).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) struct Prefix23 {
    data: [u8; HEAD_LEN],
    len: u8,
}

impl Prefix23 {
    pub(super) fn new(bytes: &[u8]) -> Self {
        debug_assert!(bytes.len() <= HEAD_LEN);
        let mut data = [0u8; HEAD_LEN];
        data[..bytes.len()].copy_from_slice(bytes);
        Self {
            data,
            len: bytes.len() as u8,
        }
    }

    #[inline]
    pub(super) fn as_slice(&self) -> &[u8] {
        &self.data[..self.len as usize]
    }
}

// `Borrow<[u8]>` (order-consistent with the derived `Ord`, verified for all
// lengths) lets level-1 lookups and ranges use a `&[u8]` slice directly — no
// throwaway `Prefix23` construction for reads.
impl std::borrow::Borrow<[u8]> for Prefix23 {
    fn borrow(&self) -> &[u8] {
        self.as_slice()
    }
}

impl AsRef<[u8]> for Prefix23 {
    fn as_ref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl PartialEq<[u8]> for Prefix23 {
    fn eq(&self, other: &[u8]) -> bool {
        self.as_slice() == other
    }
}

pub(super) enum Node<V> {
    Leaf(V),
    Branch(BTreeMap<Level2Key, V>),
}

/// A key held internally as `head ++ tail`, without reassembly. For a leaf `tail`
/// is empty; for a branch entry `head` is the 23-byte prefix and `tail` the
/// remainder. This is a **two_level-private** representation: the store hands the
/// key back to the trie walk as a [`Nibbles`] view (via [`Composite::nibbles`]),
/// so no caller outside this module ever names it.
#[derive(Clone, Copy)]
pub(super) struct Composite<'a> {
    pub(super) head: &'a [u8],
    pub(super) tail: &'a [u8],
}

impl<'a> Composite<'a> {
    /// The nibble view of this split key — the store's only key-surfacing form.
    #[inline]
    pub(super) fn nibbles(&self) -> Nibbles<'a> {
        Nibbles::from_composite(self.head, self.tail)
    }
}
