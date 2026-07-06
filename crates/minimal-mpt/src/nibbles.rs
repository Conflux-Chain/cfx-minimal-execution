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

use std::ops::Bound;

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

    /// Number of nibbles `self` and `other` share starting at index `depth`
    /// (both are compared from their own offset `depth`). Mirrors the old
    /// `common_prefix_len(first, last, depth)`.
    pub(crate) fn common_prefix_len_from(&self, other: &Nibbles<'_>, depth: usize) -> usize {
        let n = self.len().min(other.len());
        let mut i = depth;
        while i < n && self.at(i) == other.at(i) {
            i += 1;
        }
        i - depth
    }

    /// Expand to a one-nibble-per-byte `Vec` (for the shared path-merkle hasher,
    /// which is also used by the stateless oracle and so takes expanded nibbles).
    /// Path segments are short (bounded by path compression), so this is cheap.
    pub(crate) fn to_expanded(&self) -> Vec<u8> {
        (0..self.len()).map(|i| self.at(i)).collect()
    }
}

/// An owned nibble prefix of **any** length: a routing prefix and cache key.
///
/// Buffer layout: `[parity] ++ packed_pairs`, where `parity` is `0` for an even
/// nibble count and `1` for odd. The packed-pair count is `buf.len() - 1`, so
/// `parity` alone resolves the trailing half-byte ambiguity. For an odd prefix
/// the last packed byte holds the final nibble in its high half and `0` padding
/// in its low half.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) struct NibblePrefix {
    buf: Box<[u8]>,
}

impl NibblePrefix {
    /// The empty prefix (the trie root).
    pub(crate) fn empty() -> Self {
        Self {
            buf: Box::new([0u8]),
        }
    }

    /// Build from a nibble view (repacking from the view's offset).
    pub(crate) fn from_nibbles(n: Nibbles<'_>) -> Self {
        let l = n.len();
        let mut buf = Vec::with_capacity(1 + l.div_ceil(2));
        buf.push((l % 2) as u8);
        let mut i = 0;
        while i < l {
            let hi = n.at(i) << 4;
            let lo = if i + 1 < l { n.at(i + 1) } else { 0 };
            buf.push(hi | lo);
            i += 2;
        }
        Self {
            buf: buf.into_boxed_slice(),
        }
    }

    #[inline]
    fn packed(&self) -> &[u8] {
        &self.buf[1..]
    }

    #[inline]
    pub(crate) fn nibble_len(&self) -> usize {
        // packed_bytes * 2, minus 1 if odd (the padding low nibble isn't real).
        self.packed().len() * 2 - (self.buf[0] as usize)
    }

    /// Extend by one nibble, yielding the child routing prefix.
    pub(crate) fn push(&self, c: u8) -> Self {
        let c = c & 0x0f;
        if self.buf[0] == 1 {
            // Odd → fill the padding low nibble of the last byte; becomes even.
            let mut buf = self.buf.to_vec();
            let last = buf.len() - 1;
            buf[last] |= c;
            buf[0] = 0;
            Self {
                buf: buf.into_boxed_slice(),
            }
        } else {
            // Even → start a new byte with `c` in its high half; becomes odd.
            let mut buf = Vec::with_capacity(self.buf.len() + 1);
            buf.extend_from_slice(&self.buf);
            buf.push(c << 4);
            buf[0] = 1;
            Self {
                buf: buf.into_boxed_slice(),
            }
        }
    }

    /// The canonical byte key that terminates *exactly* at this prefix, if any.
    /// A key can end here only at an even nibble depth — and then the packed
    /// bytes are that canonical key. Odd prefixes never coincide with a
    /// byte-aligned key, so this is `None`.
    pub(crate) fn as_canonical_key(&self) -> Option<&[u8]> {
        (self.buf[0] == 0).then(|| self.packed())
    }

    /// Canonical-byte range `[lower, upper)` selecting exactly the entries whose
    /// (even) nibble key has this prefix. Handles the odd-parity half-byte and
    /// the `0xff` / nibble-`15` carry so callers just feed it to `BTreeMap::range`
    /// (via [`to_ref_bounds`]).
    pub(crate) fn key_bounds(&self) -> (Vec<u8>, Bound<Vec<u8>>) {
        let packed = self.packed();
        if self.buf[0] == 0 {
            // Even: keys with byte-prefix `packed`.
            (packed.to_vec(), byte_prefix_successor(packed))
        } else {
            // Odd: keys whose byte at index `m` has high nibble `h`, earlier
            // bytes equal to `packed[..m]`. `packed` already ends in `h << 4`.
            let m = packed.len() - 1;
            let h = packed[m] >> 4;
            let upper = if h < 15 {
                let mut u = packed[..m].to_vec();
                u.push((h + 1) << 4);
                Bound::Excluded(u)
            } else {
                // High nibble 15 → next value carries out of byte `m`.
                byte_prefix_successor(&packed[..m])
            };
            (packed.to_vec(), upper)
        }
    }
}

/// Smallest byte string strictly greater than every string having `p` as a
/// prefix: increment the last byte, dropping trailing `0xff`. All-`0xff` (or
/// empty) has no successor → the range is open-ended.
fn byte_prefix_successor(p: &[u8]) -> Bound<Vec<u8>> {
    let mut v = p.to_vec();
    while let Some(&last) = v.last() {
        if last == 0xff {
            v.pop();
        } else {
            let n = v.len();
            v[n - 1] = last + 1;
            return Bound::Excluded(v);
        }
    }
    Bound::Unbounded
}

/// Borrow the upper bound of [`NibblePrefix::key_bounds`] as a slice bound. The
/// lower bound is always `Included` and borrowed directly by the caller.
pub(crate) fn hi_bound_ref(hi: &Bound<Vec<u8>>) -> Bound<&[u8]> {
    match hi {
        Bound::Included(v) => Bound::Included(v.as_slice()),
        Bound::Excluded(v) => Bound::Excluded(v.as_slice()),
        Bound::Unbounded => Bound::Unbounded,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

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
            let p = NibblePrefix::from_nibbles(view);
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
        let mut p = NibblePrefix::empty();
        for (i, &c) in seq.iter().enumerate() {
            p = p.push(c);
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
            let expect = NibblePrefix::from_nibbles(Nibbles {
                bytes: Backing::One(&bytes),
                start: 0,
                end: i + 1,
            });
            assert_eq!(p, expect, "push mismatch at len {}", i + 1);
            assert_eq!(p.nibble_len(), i + 1);
        }
    }

    #[test]
    fn as_canonical_key_even_only() {
        let even = NibblePrefix::from_nibbles(Nibbles::from_bytes(&[0xab, 0xcd]));
        assert_eq!(even.as_canonical_key(), Some(&[0xab, 0xcd][..]));
        let odd = even.push(7);
        assert_eq!(odd.as_canonical_key(), None);
        assert_eq!(odd.push(9).as_canonical_key(), Some(&[0xab, 0xcd, 0x79][..]));
    }

    /// `key_bounds` must select exactly the canonical keys whose nibbles carry
    /// the prefix — for every prefix length (even and odd), including the
    /// nibble-15 / 0xff carry and the empty prefix.
    #[test]
    fn key_bounds_selects_exact_prefix_set() {
        // A spread of canonical keys, including 0xff bytes and shared prefixes.
        let keys: Vec<Vec<u8>> = vec![
            vec![],
            vec![0x00],
            vec![0x0f],
            vec![0x10],
            vec![0xff],
            vec![0xff, 0x00],
            vec![0xff, 0xff],
            vec![0x12, 0x34],
            vec![0x12, 0x35],
            vec![0x12, 0xf0],
            vec![0x12, 0xff],
            vec![0xab, 0xcd, 0xef],
        ];
        let map: BTreeMap<Box<[u8]>, ()> =
            keys.iter().map(|k| (k.clone().into_boxed_slice(), ())).collect();

        // Enumerate every nibble prefix (all lengths) of every key and check.
        for k in &keys {
            let nib = expand(k);
            for plen in 0..=nib.len() {
                let view = Nibbles {
                    bytes: Backing::One(k),
                    start: 0,
                    end: plen,
                };
                let prefix = NibblePrefix::from_nibbles(view);
                let (lo, hi) = prefix.key_bounds();
                let selected: Vec<&[u8]> = map
                    .range::<[u8], _>((Bound::Included(lo.as_slice()), hi_bound_ref(&hi)))
                    .map(|(kk, _)| kk.as_ref())
                    .collect();
                // Reference: keys whose nibble expansion starts with `nib[..plen]`,
                // in byte-sorted order to match the BTreeMap range iteration.
                let mut want: Vec<&[u8]> = keys
                    .iter()
                    .filter(|kk| {
                        let e = expand(kk);
                        e.len() >= plen && e[..plen] == nib[..plen]
                    })
                    .map(|kk| kk.as_slice())
                    .collect();
                want.sort_unstable();
                assert_eq!(selected, want, "prefix {:?} of key {:?}", &nib[..plen], k);
            }
        }
    }
}
