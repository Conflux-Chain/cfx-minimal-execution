//! Prefix-range machinery of the two-level entry store: the forward flattening
//! iterator [`PrefixRange`] and the parity-aware upper-bound helpers
//! [`byte_successor`] / [`prefix_upper`]. Split out of `two_level/mod.rs`.

use super::{under_prefix, Anchor, Composite, Level2Key, Node, Prefix23, HEAD_LEN};
use crate::nibbles::{NibbleRef, Nibbles};
use std::collections::btree_map;
use std::ops::Bound;

/// Smallest byte string strictly greater than every string having `p` as a
/// prefix: increment the last non-`0xff` byte, dropping trailing `0xff`. An
/// all-`0xff` (or empty) `p` has no successor → the range is open-ended.
pub(super) fn byte_successor(p: &[u8]) -> Bound<Anchor> {
    let mut v = Anchor::new();
    v.extend_from_slice(p);
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

/// Exclusive upper byte-bound of the packed prefix `packed` (parity `even`): the
/// first canonical key NOT under it. Parity-aware — an odd prefix's trailing
/// half-nibble `h` ends the range at high-nibble `h + 1`, carrying out of the
/// byte when `h == 15`.
pub(super) fn prefix_upper(packed: &[u8], even: bool) -> Bound<Anchor> {
    if even {
        return byte_successor(packed);
    }
    let (last, rest) = packed.split_last().expect("odd prefix has ≥1 byte");
    let h = last >> 4;
    if h < 15 {
        let mut u = Anchor::new();
        u.extend_from_slice(rest);
        u.push((h + 1) << 4);
        Bound::Excluded(u)
    } else {
        byte_successor(rest)
    }
}

/// Forward flattening iterator over the entries carrying a nibble prefix (or,
/// when `prefix` is `None`, every entry): walks level-1 nodes, expanding each
/// branch's level-2 map in order, and stops at the first key past the prefix.
/// The prefix is borrowed (`'b`); it materializes no byte bounds.
pub(crate) struct PrefixRange<'a, 'b, V> {
    pub(super) l1: btree_map::Range<'a, Prefix23, Node<V>>,
    pub(super) branch: Option<(&'a [u8], btree_map::Range<'a, Level2Key, V>)>,
    pub(super) prefix: Option<NibbleRef<'b>>,
    pub(super) done: bool,
}

impl<'a, 'b, V> Iterator for PrefixRange<'a, 'b, V> {
    type Item = (Nibbles<'a>, &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        loop {
            if let Some((head, bi)) = self.branch.as_mut() {
                let head = *head;
                match bi.next() {
                    Some((l2, v)) => {
                        if self.prefix.is_some_and(|p| !under_prefix(p, head, l2)) {
                            self.done = true;
                            return None;
                        }
                        return Some((Composite { head, tail: l2.as_slice() }.nibbles(), v));
                    }
                    None => self.branch = None,
                }
            }
            match self.l1.next()? {
                (p, Node::Leaf(v)) => {
                    let head = p.as_slice();
                    if self.prefix.is_some_and(|px| !under_prefix(px, head, &[])) {
                        self.done = true;
                        return None;
                    }
                    return Some((Composite { head, tail: &[] }.nibbles(), v));
                }
                (p, Node::Branch(b)) => {
                    let head = p.as_slice();
                    // Decide whether this branch carries matching keys, and the
                    // level-2 lower bound (the tail floor for a boundary branch).
                    let l2_lo: Bound<&[u8]> = match self.prefix {
                        None => Bound::Unbounded,
                        Some(px) => {
                            let packed = px.packed();
                            if packed.len() > HEAD_LEN {
                                // Prefix reaches into the tail: only the node
                                // `head == packed[..23]` qualifies; seek level-2
                                // to the tail floor. Any other head is past it.
                                if head == &packed[..HEAD_LEN] {
                                    Bound::Included(&packed[HEAD_LEN..])
                                } else {
                                    self.done = true;
                                    return None;
                                }
                            } else if under_prefix(px, head, &[]) {
                                // Prefix lies within the head → whole branch matches.
                                Bound::Unbounded
                            } else {
                                self.done = true;
                                return None;
                            }
                        }
                    };
                    self.branch = Some((head, b.range::<[u8], _>((l2_lo, Bound::Unbounded))));
                }
            }
        }
    }
}
