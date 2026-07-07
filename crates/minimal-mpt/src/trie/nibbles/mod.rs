//! Packed nibble representations for the incremental trie.
//!
//! The trie routes 16-way, so it works in **nibbles** (4-bit units). The naive
//! representation expands each key byte into two `u8`s (one nibble each) — which
//! *doubles* the heap footprint of the ~250M-entry snapshot map and the ~99M
//! routing-prefix cache. This module stores nibbles **packed 2-per-byte** and
//! encapsulates every nibble-level operation, so the rest of the crate never
//! indexes raw bytes as nibbles.
//!
//! Two observations drive the two types here:
//!
//! * **Entry keys are byte-aligned → their nibble length is always even.** An
//!   even nibble sequence packed 2-per-byte *is exactly the original canonical
//!   byte key*, and nibble-lexicographic order over even keys equals byte order.
//!   So entry keys need no dedicated type at all: the map is keyed directly on
//!   canonical bytes (`Box<[u8]>`), and [`Nibbles`] is a zero-cost borrowed view
//!   used only for the routing/compression reads.
//!
//! * **Routing prefixes / cache keys can land at an odd nibble depth.** Packing
//!   an odd-length sequence leaves the last byte's low nibble unused — and a bare
//!   byte buffer can't tell "trailing real 0 nibble" from "unused padding". So
//!   [`NibblePrefix`] carries one **parity byte** to disambiguate (the buffer's
//!   own length already gives the packed byte count, so a single byte suffices).
//!   The cache is a `HashMap`, so this type needs only `Hash`/`Eq`, never `Ord`.

use smallvec::SmallVec;
use std::borrow::Borrow;

/// The byte backing of a [`Nibbles`] view: either one contiguous slice, or two
/// slices to be read as their concatenation. The two-level entries store keeps a
/// key as `head` (shared prefix, ≤23 B) + `tail` (per-entry remainder), and this
/// lets the nibble routing read that split key with no allocation.
#[derive(Clone, Copy)]
enum Backing<'a> {
    One(&'a [u8]),
    Two(&'a [u8], &'a [u8]),
}

impl Backing<'_> {
    #[inline]
    fn byte_at(&self, i: usize) -> u8 {
        match self {
            Backing::One(b) => b[i],
            Backing::Two(h, t) => match i.checked_sub(h.len()) {
                Some(j) => t[j],
                None => h[i],
            },
        }
    }
}

/// Borrowed nibble view over a (possibly split) byte buffer, spanning logical
/// nibbles `[start, end)`. Zero-cost: it holds the backing and two offsets. All
/// the "split into nibbles / take a prefix" logic lives here.
#[derive(Clone, Copy)]
pub(crate) struct Nibbles<'a> {
    bytes: Backing<'a>,
    start: usize,
    end: usize,
}

impl<'a> Nibbles<'a> {
    /// View the full (even-length) nibble sequence of a canonical byte key.
    #[inline]
    pub(crate) fn from_bytes(bytes: &'a [u8]) -> Self {
        Self {
            bytes: Backing::One(bytes),
            start: 0,
            end: bytes.len() * 2,
        }
    }

    /// View the nibble sequence of a key split across `head ++ tail`.
    #[inline]
    pub(crate) fn from_composite(head: &'a [u8], tail: &'a [u8]) -> Self {
        Self {
            bytes: Backing::Two(head, tail),
            start: 0,
            end: (head.len() + tail.len()) * 2,
        }
    }

    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.end - self.start
    }

    #[inline]
    pub(crate) fn at(&self, i: usize) -> u8 {
        let n = self.start + i;
        let b = self.bytes.byte_at(n / 2);
        if n % 2 == 0 {
            b >> 4
        } else {
            b & 0x0f
        }
    }

    /// Sub-view over logical nibbles `[a, b)` of this view.
    #[inline]
    pub(crate) fn sub(&self, a: usize, b: usize) -> Nibbles<'a> {
        debug_assert!(a <= b && self.start + b <= self.end);
        Nibbles {
            bytes: self.bytes,
            start: self.start + a,
            end: self.start + b,
        }
    }

    /// Nibbles of this view from logical index `from` to the end, reading each
    /// backing byte **once** (splitting it into its two nibbles) rather than
    /// re-dispatching through `byte_at` per nibble. The batch primitive the
    /// expansion/comparison helpers below build on.
    fn nibbles_from(self, from: usize) -> impl Iterator<Item = u8> + 'a {
        let (s, e) = (self.start + from, self.end);
        (s / 2..e.div_ceil(2))
            .flat_map(move |bi| {
                let b = self.bytes.byte_at(bi);
                [b >> 4, b & 0x0f]
            })
            .skip(s % 2) // drop the leading half-nibble when `s` is odd
            .take(e - s)
    }

    /// Number of nibbles `self` and `other` share starting at index `depth`
    /// (both compared from their own offset `depth`).
    pub(crate) fn common_prefix_len_from(&self, other: &Nibbles<'_>, depth: usize) -> usize {
        self.nibbles_from(depth)
            .zip(other.nibbles_from(depth))
            .take_while(|(a, b)| a == b)
            .count()
    }

    /// Expand to a one-nibble-per-byte `Vec` (for the shared path-merkle hasher,
    /// which is also used by the stateless oracle and so takes expanded nibbles).
    /// Path segments are short (bounded by path compression), so this is cheap.
    pub(crate) fn to_expanded(&self) -> Vec<u8> {
        self.nibbles_from(0).collect()
    }

    /// Repack an **even-length** nibble view into its canonical byte key. For an
    /// entry surfaced from the store as `head ++ tail` this reproduces the exact
    /// concatenation `[head, tail].concat()` (byte-aligned keys only), letting the
    /// snapshot read/persist paths reassemble the key from the nibble view without
    /// naming the store's internal split representation.
    pub(crate) fn to_canonical_bytes(&self) -> Vec<u8> {
        use itertools::Itertools;
        debug_assert!(self.len() % 2 == 0, "canonical key must be byte-aligned");
        self.nibbles_from(0)
            .tuples()
            .map(|(hi, lo)| (hi << 4) | lo)
            .collect()
    }

    /// **Value** equality of the nibble path: `true` iff `self` and `other` yield
    /// the same nibble sequence (same length, same nibbles), independent of
    /// backing buffer or offset. Not representation identity — the singleton-node
    /// check relies on this comparing the logical key, not the split.
    pub(crate) fn logical_eq(&self, other: &Nibbles<'_>) -> bool {
        self.len() == other.len() && self.nibbles_from(0).eq(other.nibbles_from(0))
    }
}

/// A routing nibble prefix, in the `[parity] ++ packed_pairs` byte layout:
/// `parity` is `0` for an even nibble count and `1` for odd, so it resolves the
/// trailing half-byte ambiguity of the packed pairs (an odd prefix's last byte
/// holds the final nibble in its high half, `0` in its low). Two representations
/// share this layout — [`NibblePrefix`] owns it (`Box<[u8]>`, the cache key) and
/// [`PrefixBuf`] holds it on the stack (the transient key built during the walk)
/// — and **all the read ops live on the borrowed [`NibbleRef`]** so both use them
/// without copying.
pub(crate) type PrefixBuf = SmallVec<u8, 64>;

/// A borrowed `[parity ++ packed]` prefix. `Copy`; carries every read op.
#[derive(Clone, Copy)]
pub(crate) struct NibbleRef<'a>(&'a [u8]);

impl<'a> NibbleRef<'a> {
    /// View borrowed `[parity ++ packed]` bytes as a prefix.
    #[inline]
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        NibbleRef(bytes)
    }

    /// The empty prefix (the trie root): parity `0`, no packed bytes.
    #[inline]
    pub(crate) fn empty() -> Self {
        NibbleRef(&[0u8])
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
pub(crate) fn pack_nibbles(n: Nibbles<'_>) -> PrefixBuf {
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
/// as a map key; every read goes through [`NibblePrefix::as_ref`] → [`NibbleRef`],
/// and lookups borrow it as `&[u8]` (see the [`Borrow`] impl) so the walk never
/// constructs one just to `get`/`remove`.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) struct NibblePrefix {
    buf: Box<[u8]>,
}

impl NibblePrefix {
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
// `PrefixBuf`) instead of an owned `NibblePrefix`. Sound because the derived
// `Hash`/`Eq` act on `buf`, matching `<[u8]>`'s (a `Box<[u8]>` hashes as its
// slice), so `hash(&self) == hash(self.borrow())`.
impl Borrow<[u8]> for NibblePrefix {
    #[inline]
    fn borrow(&self) -> &[u8] {
        &self.buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference expansion: canonical bytes → one-nibble-per-byte vec.
    fn expand(bytes: &[u8]) -> Vec<u8> {
        let mut v = Vec::with_capacity(bytes.len() * 2);
        for b in bytes {
            v.push(b >> 4);
            v.push(b & 0x0f);
        }
        v
    }

    #[test]
    fn nibble_view_matches_expansion() {
        let key = [0x12u8, 0xab, 0xff, 0x00];
        let n = Nibbles::from_bytes(&key);
        let exp = expand(&key);
        assert_eq!(n.len(), exp.len());
        for i in 0..n.len() {
            assert_eq!(n.at(i), exp[i]);
        }
        // Sub-view + expansion round-trip.
        assert_eq!(n.sub(2, 6).to_expanded(), &exp[2..6]);
    }

    #[test]
    fn prefix_roundtrip_and_len_parity() {
        // Even and odd nibble sequences repack + report length correctly.
        for raw in [
            vec![],
            vec![1u8],
            vec![1, 2],
            vec![1, 2, 3],
            vec![15, 0, 15, 0, 15],
            vec![0, 0, 0, 0, 0, 0],
        ] {
            let bytes: Vec<u8> = {
                // Pack `raw` (nibbles) into a byte buffer to build a view from.
                let mut b = Vec::new();
                let mut i = 0;
                while i < raw.len() {
                    let hi = raw[i] << 4;
                    let lo = if i + 1 < raw.len() { raw[i + 1] } else { 0 };
                    b.push(hi | lo);
                    i += 2;
                }
                b
            };
            let view = Nibbles {
                bytes: Backing::One(&bytes),
                start: 0,
                end: raw.len(),
            };
            let buf = pack_nibbles(view);
            let p = NibbleRef::new(&buf);
            assert_eq!(p.nibble_len(), raw.len(), "len for {raw:?}");
            let packed = p.packed();
            let back: Vec<u8> = (0..p.nibble_len())
                .map(|i| {
                    let b = packed[i / 2];
                    if i % 2 == 0 {
                        b >> 4
                    } else {
                        b & 0x0f
                    }
                })
                .collect();
            assert_eq!(back, raw, "roundtrip for {raw:?}");
        }
    }

    #[test]
    fn push_builds_prefix_incrementally() {
        // Building nibble-by-nibble equals building from the whole sequence.
        let seq = [3u8, 14, 0, 9, 1, 15, 15, 2];
        let mut buf = empty_prefix_buf();
        for (i, &c) in seq.iter().enumerate() {
            push_nibble(&mut buf, c);
            let bytes = {
                let mut b = Vec::new();
                let mut j = 0;
                let sub = &seq[..=i];
                while j < sub.len() {
                    let hi = sub[j] << 4;
                    let lo = if j + 1 < sub.len() { sub[j + 1] } else { 0 };
                    b.push(hi | lo);
                    j += 2;
                }
                b
            };
            let expect = pack_nibbles(Nibbles {
                bytes: Backing::One(&bytes),
                start: 0,
                end: i + 1,
            });
            assert_eq!(buf, expect, "push mismatch at len {}", i + 1);
            assert_eq!(NibbleRef::new(&buf).nibble_len(), i + 1);
        }
    }

    #[test]
    fn as_canonical_key_even_only() {
        let mut buf = pack_nibbles(Nibbles::from_bytes(&[0xab, 0xcd]));
        assert_eq!(NibbleRef::new(&buf).as_canonical_key(), Some(&[0xab, 0xcd][..]));
        push_nibble(&mut buf, 7);
        assert_eq!(NibbleRef::new(&buf).as_canonical_key(), None);
        push_nibble(&mut buf, 9);
        assert_eq!(
            NibbleRef::new(&buf).as_canonical_key(),
            Some(&[0xab, 0xcd, 0x79][..])
        );
    }
}
