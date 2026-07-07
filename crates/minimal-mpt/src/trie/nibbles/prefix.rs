//! Routing-prefix representations built on the packed nibble layout.
//!
//! Split out of `nibbles/mod.rs`: the borrowed [`PrefixRef`] read view, the owned
//! [`PrefixKey`] cache key, the stack [`PrefixBuf`], and the `[parity ++ packed]`
//! construction/mutation helpers ([`pack_nibbles`], [`empty_prefix_buf`],
//! [`push_nibble`]). `NibbleCursor` itself stays in the parent module.

use super::NibbleCursor;
use smallvec::SmallVec;
use std::borrow::Borrow;

/// A routing nibble prefix, in the `[parity] ++ packed_pairs` byte layout:
/// `parity` is `0` for an even nibble count and `1` for odd, so it resolves the
/// trailing half-byte ambiguity of the packed pairs (an odd prefix's last byte
/// holds the final nibble in its high half, `0` in its low). Two representations
/// share this layout — [`PrefixKey`] owns it (`Box<[u8]>`, the cache key) and
/// [`PrefixBuf`] holds it on the stack (the transient key built during the walk)
/// — and **all the read ops live on the borrowed [`PrefixRef`]** so both use them
/// without copying.
pub(crate) type PrefixBuf = SmallVec<u8, 64>;

/// A borrowed `[parity ++ packed]` prefix. `Copy`; carries every read op.
#[derive(Clone, Copy)]
pub(crate) struct PrefixRef<'a>(&'a [u8]);

impl<'a> PrefixRef<'a> {
    /// View borrowed `[parity ++ packed]` bytes as a prefix.
    #[inline]
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        PrefixRef(bytes)
    }

    /// The empty prefix (the trie root): parity `0`, no packed bytes.
    #[inline]
    pub(crate) fn empty() -> Self {
        PrefixRef(&[0u8])
    }

    /// The `[parity ++ packed]` bytes — the cache-lookup key (`get`/`remove`).
    #[inline]
    pub(crate) fn as_bytes(&self) -> &'a [u8] {
        self.0
    }

    /// The packed bytes (no parity byte). Even → the canonical prefix bytes; odd
    /// → last byte holds the trailing nibble high, `0` low. Also the inclusive
    /// lower byte-bound of the prefix's key range.
    #[inline]
    pub(crate) fn packed(&self) -> &'a [u8] {
        &self.0[1..]
    }

    /// Whether the prefix has an even nibble count.
    #[inline]
    pub(crate) fn is_even(&self) -> bool {
        self.0[0] == 0
    }

    #[inline]
    pub(crate) fn nibble_len(&self) -> usize {
        // packed_bytes * 2, minus 1 if odd (the padding low nibble isn't real).
        self.packed().len() * 2 - (self.0[0] as usize)
    }

    /// The canonical byte key ending *exactly* at this prefix, if any — possible
    /// only at an even (byte-aligned) depth, where the packed bytes are that key.
    #[inline]
    pub(crate) fn as_canonical_key(&self) -> Option<&'a [u8]> {
        (self.0[0] == 0).then(|| self.packed())
    }
}

/// A fresh stack [`PrefixBuf`] holding a nibble view in `[parity ++ packed]`
/// layout (padding a trailing lone nibble with `0`). No per-nibble `at`:
/// `nibbles_from` reads each backing byte once, `tuples` pairs the stream.
pub(crate) fn pack_nibbles(n: NibbleCursor<'_>) -> PrefixBuf {
    use itertools::Itertools;
    let l = n.len();
    std::iter::once((l % 2) as u8)
        .chain(
            n.nibbles_from(0)
                .chain(std::iter::repeat(0u8).take(l % 2))
                .tuples()
                .map(|(hi, lo)| (hi << 4) | lo),
        )
        .collect()
}

/// A fresh empty [`PrefixBuf`] (the trie root: just the parity byte).
pub(crate) fn empty_prefix_buf() -> PrefixBuf {
    let mut buf = PrefixBuf::new();
    buf.push(0);
    buf
}

/// Extend a `[parity ++ packed]` [`PrefixBuf`] by one nibble **in place** — the
/// per-nibble step of the trie walk / dirty-eviction, with no allocation.
pub(crate) fn push_nibble(buf: &mut PrefixBuf, c: u8) {
    let c = c & 0x0f;
    if buf[0] == 1 {
        // Odd → fill the padding low nibble of the last byte; becomes even.
        let last = buf.len() - 1;
        buf[last] |= c;
        buf[0] = 0;
    } else {
        // Even → start a new byte with `c` in its high half; becomes odd.
        buf.push(c << 4);
        buf[0] = 1;
    }
}

/// An owned `[parity ++ packed]` prefix: the routing-cache key. Kept owned only
/// as a map key; every read goes through [`PrefixKey::as_ref`] → [`PrefixRef`],
/// and lookups borrow it as `&[u8]` (see the [`Borrow`] impl) so the walk never
/// constructs one just to `get`/`remove`.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) struct PrefixKey {
    buf: Box<[u8]>,
}

impl PrefixKey {
    /// Own a `[parity ++ packed]` byte layout (e.g. a transient [`PrefixBuf`]) as
    /// a cache key — the only place the walk allocates a prefix, on `insert`.
    #[inline]
    pub(crate) fn from_bytes(bytes: &[u8]) -> Self {
        Self {
            buf: Box::from(bytes),
        }
    }
}

// The cache can look up / evict by a borrowed `&[u8]` key (the walk's stack
// `PrefixBuf`) instead of an owned `PrefixKey`. Sound because the derived
// `Hash`/`Eq` act on `buf`, matching `<[u8]>`'s (a `Box<[u8]>` hashes as its
// slice), so `hash(&self) == hash(self.borrow())`.
impl Borrow<[u8]> for PrefixKey {
    #[inline]
    fn borrow(&self) -> &[u8] {
        &self.buf
    }
}
