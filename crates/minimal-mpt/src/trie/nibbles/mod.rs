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
//!   canonical bytes (`Box<[u8]>`), and [`NibbleCursor`] is a zero-cost borrowed view
//!   used only for the routing/compression reads.
//!
//! * **Routing prefixes / cache keys can land at an odd nibble depth.** Packing
//!   an odd-length sequence leaves the last byte's low nibble unused — and a bare
//!   byte buffer can't tell "trailing real 0 nibble" from "unused padding". So
//!   [`PrefixKey`] carries one **parity byte** to disambiguate (the buffer's
//!   own length already gives the packed byte count, so a single byte suffices).
//!   The cache is a `HashMap`, so this type needs only `Hash`/`Eq`, never `Ord`.

mod prefix;

pub(crate) use prefix::{empty_prefix_buf, pack_nibbles, push_nibble, PrefixKey, PrefixRef};

/// The byte backing of a [`NibbleCursor`] view: either one contiguous slice, or two
/// slices to be read as their concatenation. The two-level entries store keeps a
/// key as `head` (shared prefix, ≤23 B) + `tail` (per-entry remainder), and this
/// lets the nibble routing read that split key with no allocation.
#[derive(Clone, Copy)]
enum Backing<'a> {
    One(&'a [u8]),
    Two(&'a [u8], &'a [u8]),
}

impl<'a> Backing<'a> {
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

    /// The backing as two byte segments to be read as their concatenation
    /// (`One` yields an empty second segment).
    #[inline]
    fn segments(&self) -> (&'a [u8], &'a [u8]) {
        match self {
            Backing::One(b) => (b, &[]),
            Backing::Two(h, t) => (h, t),
        }
    }
}

/// Are the concatenations `x0 ++ x1` and `y0 ++ y1` equal, compared piecewise
/// (no reassembly)? Slices from the same store often alias (two entries under
/// one level-1 node share the identical `head` slice), which `==` short-circuits
/// on pointer+length before falling back to memcmp.
fn concat_eq(x0: &[u8], x1: &[u8], y0: &[u8], y1: &[u8]) -> bool {
    if x0.len() + x1.len() != y0.len() + y1.len() {
        return false;
    }
    // Same split — the common case (keys from one store share the head/tail
    // cut): two whole-segment memcmps, nothing to realign.
    if x0.len() == y0.len() {
        return x0 == y0 && x1 == y1;
    }
    // Different splits: WLOG x cuts no later than y, then realign both to the
    // three ranges [0, p), [p, q), [q, n) — whole-segment memcmp only.
    let (x0, x1, y0, y1) = if x0.len() < y0.len() {
        (x0, x1, y0, y1)
    } else {
        (y0, y1, x0, x1)
    };
    let (y0a, y0b) = y0.split_at(x0.len());
    let (x1a, x1b) = x1.split_at(y0b.len());
    x0 == y0a && x1a == y0b && x1b == y1
}

/// Borrowed nibble view over a (possibly split) byte buffer, spanning logical
/// nibbles `[start, end)`. Zero-cost: it holds the backing and two offsets. All
/// the "split into nibbles / take a prefix" logic lives here.
#[derive(Clone, Copy)]
pub(crate) struct NibbleCursor<'a> {
    bytes: Backing<'a>,
    start: usize,
    end: usize,
}

impl<'a> NibbleCursor<'a> {
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
    pub(crate) fn sub(&self, a: usize, b: usize) -> NibbleCursor<'a> {
        debug_assert!(a <= b && self.start + b <= self.end);
        NibbleCursor {
            bytes: self.bytes,
            start: self.start + a,
            end: self.start + b,
        }
    }

    /// The backing bytes from byte index `bi` on, streamed across both segments.
    /// Pure iterator advance — no per-byte bounds check or backing dispatch
    /// (`skip` lands via the slice iterators' O(1) `nth`).
    #[inline]
    pub(in crate::trie::nibbles) fn bytes_from(self, bi: usize) -> impl Iterator<Item = u8> + 'a {
        let (a, b) = self.bytes.segments();
        a.iter().chain(b).copied().skip(bi)
    }

    /// The backing bytes from the view's start, when it is **byte-aligned**
    /// (even `start`); `None` for odd-offset views. At byte alignment the
    /// packed nibble pairs are the backing bytes themselves, so consumers
    /// (e.g. [`super::prefix::pack_nibbles`]) copy bytes instead of re-deriving
    /// the pairing nibble-by-nibble.
    #[inline]
    pub(in crate::trie::nibbles) fn aligned_bytes(self) -> Option<impl Iterator<Item = u8> + 'a> {
        (self.start % 2 == 0).then(move || self.bytes_from(self.start / 2))
    }

    /// NibbleCursor of this view from logical index `from` to the end, reading each
    /// backing byte **once** (splitting it into its two nibbles) rather than
    /// re-dispatching through `byte_at` per nibble. The batch primitive the
    /// expansion/comparison helpers below build on.
    pub(in crate::trie::nibbles) fn nibbles_from(
        self,
        from: usize,
    ) -> impl Iterator<Item = u8> + 'a {
        let (s, e) = (self.start + from, self.end);
        self.bytes_from(s / 2)
            .flat_map(|b| [b >> 4, b & 0x0f])
            .skip(s % 2) // drop the leading half-nibble when `s` is odd
            .take(e - s)
    }

    /// Number of nibbles `self` and `other` share starting at index `depth`
    /// (both compared from their own offset `depth`). When both sides land at
    /// the same byte parity (always: the walk compares keys anchored at one
    /// node, same `start` and `depth`), the scan is whole-byte — one compare
    /// per two nibbles, plus a half-byte fixup at each end; only mixed-parity
    /// views walk nibbles.
    pub(crate) fn common_prefix_len_from(&self, other: &NibbleCursor<'_>, depth: usize) -> usize {
        let (s1, s2) = (self.start + depth, other.start + depth);
        if s1 % 2 != s2 % 2 {
            return self
                .nibbles_from(depth)
                .zip(other.nibbles_from(depth))
                .take_while(|(a, b)| a == b)
                .count();
        }
        let remaining = (self.len() - depth).min(other.len() - depth);
        let mut n = 0;
        // Odd anchor: settle the lone low nibble first, then bytes align.
        if s1 % 2 == 1 {
            if remaining == 0 || self.at(depth) != other.at(depth) {
                return 0;
            }
            n = 1;
        }
        for (b1, b2) in self
            .bytes_from((s1 + n) / 2)
            .zip(other.bytes_from((s2 + n) / 2))
        {
            if n >= remaining {
                break;
            }
            if b1 == b2 {
                n += 2;
            } else {
                if b1 >> 4 == b2 >> 4 {
                    n += 1;
                }
                break;
            }
        }
        n.min(remaining)
    }

    /// Expand to a one-nibble-per-byte `Vec` (for the shared path-merkle hasher,
    /// which is also used by the stateless oracle and so takes expanded nibbles).
    /// Path segments are short (bounded by path compression), so this is cheap.
    pub(crate) fn to_expanded(&self) -> Vec<u8> {
        self.nibbles_from(0).collect()
    }

    /// The backing byte segments, when the view spans the **whole backing**
    /// (`start == 0`, `end` at the last nibble). Packed nibbles at byte
    /// alignment *are* the canonical bytes (see module doc), so a full view
    /// needs no nibble work at all — and full-key views (`from_bytes`/
    /// `from_composite`, the only ones equality/reassembly run on) always
    /// qualify. `None` for sub-views (routing prefixes), which genuinely need
    /// nibble granularity. No slicing arithmetic: the segments are returned
    /// as-is. Crate-visible so bulk consumers (the cold rebuild's entry array)
    /// can hold a full key as two bare slices — 48 B/entry instead of a 72 B
    /// cursor — and rebuild the view on demand via [`Self::from_composite`].
    #[inline]
    pub(crate) fn full_view_segments(&self) -> Option<(&'a [u8], &'a [u8])> {
        let (a, b) = self.bytes.segments();
        (self.start == 0 && self.end == (a.len() + b.len()) * 2).then_some((a, b))
    }

    /// Repack an **even-length** nibble view into its canonical byte key. For an
    /// entry surfaced from the store as `head ++ tail` this reproduces the exact
    /// concatenation `[head, tail].concat()` (byte-aligned keys only), letting the
    /// snapshot read/persist paths reassemble the key from the nibble view without
    /// naming the store's internal split representation.
    pub(crate) fn to_canonical_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.len() / 2);
        self.extend_canonical_into(&mut out);
        out
    }

    /// [`Self::to_canonical_bytes`] into a caller-owned buffer, so bulk visitors
    /// (the checkpoint save walks every snapshot entry) can reuse one allocation.
    pub(crate) fn extend_canonical_into(&self, out: &mut Vec<u8>) {
        debug_assert!(self.len() % 2 == 0, "canonical key must be byte-aligned");
        if let Some((a, b)) = self.full_view_segments() {
            out.extend_from_slice(a);
            out.extend_from_slice(b);
        } else {
            // Odd-offset view: genuinely repack nibble pairs. No production key
            // path lands here (canonical keys are byte-aligned).
            use itertools::Itertools;
            out.extend(self.nibbles_from(0).tuples().map(|(hi, lo)| (hi << 4) | lo));
        }
    }

    /// **Value** equality of the nibble path: `true` iff `self` and `other` yield
    /// the same nibble sequence (same length, same nibbles), independent of
    /// backing buffer or offset. Not representation identity — the singleton-node
    /// check relies on this comparing the logical key, not the split. Full-key
    /// views (the only callers) compare as bytes, piecewise over the backing
    /// segments; only sub-views walk nibbles.
    pub(crate) fn logical_eq(&self, other: &NibbleCursor<'_>) -> bool {
        if self.len() != other.len() {
            return false;
        }
        match (self.full_view_segments(), other.full_view_segments()) {
            (Some((a1, b1)), Some((a2, b2))) => concat_eq(a1, b1, a2, b2),
            _ => self.nibbles_from(0).eq(other.nibbles_from(0)),
        }
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
        let n = NibbleCursor::from_bytes(&key);
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
            let view = NibbleCursor {
                bytes: Backing::One(&bytes),
                start: 0,
                end: raw.len(),
            };
            let buf = pack_nibbles(view);
            let p = PrefixRef::new(&buf);
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
            let expect = pack_nibbles(NibbleCursor {
                bytes: Backing::One(&bytes),
                start: 0,
                end: i + 1,
            });
            assert_eq!(buf, expect, "push mismatch at len {}", i + 1);
            assert_eq!(PrefixRef::new(&buf).nibble_len(), i + 1);
        }
    }

    #[test]
    fn as_canonical_key_even_only() {
        let mut buf = pack_nibbles(NibbleCursor::from_bytes(&[0xab, 0xcd]));
        assert_eq!(
            PrefixRef::new(&buf).as_canonical_key(),
            Some(&[0xab, 0xcd][..])
        );
        push_nibble(&mut buf, 7);
        assert_eq!(PrefixRef::new(&buf).as_canonical_key(), None);
        push_nibble(&mut buf, 9);
        assert_eq!(
            PrefixRef::new(&buf).as_canonical_key(),
            Some(&[0xab, 0xcd, 0x79][..])
        );
    }
}
