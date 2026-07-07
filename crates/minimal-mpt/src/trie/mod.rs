pub(crate) mod nibbles;
pub(crate) mod reader;
pub(crate) mod root;
pub(crate) mod store;
mod value;

pub use value::{MptValue, TrieValue};

pub use crate::merkle::oracle::trie_root;
#[cfg(feature = "verify-incremental")]
pub(crate) use crate::merkle::oracle::{bytes_to_nibbles, root_for_entries};
pub(crate) use crate::merkle::rules::{compute_node_merkle, compute_path_merkle, keccak, CHILDREN};

use crate::entries::Entries;
use crate::primitives::H256;
use reader::TrieStore;
use root::{CachePolicy, RootEngine};
use std::collections::BTreeMap;

/// A store composed with an incremental [`RootEngine`], fixing the invariant that
/// **every write marks the engine dirty**: `insert` / `remove` update both the
/// `store` and the engine's pending set, so `root` only re-hashes changed
/// subtrees. The engine's [`CachePolicy`] is fixed when the trie is built (delta
/// layers use `Full`, the snapshot uses `SkipSingleton`); it never varies per
/// call. The engine sees the store only through [`TrieReader`] (merkle bytes), so
/// it never names the value type `V`.
///
/// Generic over the store `S` (the sole store today is the two-level
/// [`Entries`]); the public [`IncrementalTrie`] newtype pins `S` and keeps the
/// store/trait bounds out of the crate's public surface.
///
/// [`RootEngine`]: root::RootEngine
/// [`TrieReader`]: reader::TrieReader
#[derive(Clone, Debug)]
pub(crate) struct AuthenticatedTrie<S> {
    store: S,
    engine: RootEngine,
}

impl<S> AuthenticatedTrie<S> {
    /// Borrow the underlying store (value-typed reads live on the store / layer,
    /// not on the trie).
    #[inline]
    pub(crate) fn store(&self) -> &S {
        &self.store
    }

    /// Consume the trie, yielding its store (dropping the cache/dirty engine). The
    /// delta layer uses this to MOVE its finished store into the intermediate layer
    /// at a period boundary — no re-materialization of the entries.
    #[inline]
    pub(crate) fn into_store(self) -> S {
        self.store
    }

    /// Wrap an already-populated store with a fresh `Full`-policy engine (empty
    /// dirty set; the first `root` is a full recompute that warms the cache).
    /// Matches `from_delta`'s "build the store, don't mark each key dirty"
    /// semantics for any store backend — the delta layer's load path.
    #[inline]
    pub(crate) fn from_store(store: S) -> Self {
        Self {
            store,
            engine: RootEngine::new(CachePolicy::Full),
        }
    }
}

impl<S: Default> AuthenticatedTrie<S> {
    /// An empty trie with the given retention policy.
    fn empty(policy: CachePolicy) -> Self {
        Self {
            store: S::default(),
            engine: RootEngine::new(policy),
        }
    }
}

/// Default trie is a **delta** trie: empty store, `Full` cache policy. The
/// snapshot layer, which needs `SkipSingleton`, constructs explicitly and never
/// relies on this default (see `AuthenticatedTrie::<Entries<_>>::empty_snapshot`).
impl<S: Default> Default for AuthenticatedTrie<S> {
    fn default() -> Self {
        Self::empty(CachePolicy::Full)
    }
}

impl<S: TrieStore> AuthenticatedTrie<S> {
    /// Insert / overwrite the value for `key`, marking the subtree dirty.
    pub(crate) fn insert(&mut self, key: &[u8], value: S::Value) {
        self.engine.mark_dirty(key);
        self.store.insert(key, value);
    }

    /// Remove `key`, marking the subtree dirty.
    pub(crate) fn remove(&mut self, key: &[u8]) {
        self.store.remove(key);
        self.engine.mark_dirty(key);
    }

    /// Drop all entries, cached hashes and pending dirty keys (e.g. on a delta
    /// reset at snapshot rotation).
    pub(crate) fn clear(&mut self) {
        self.store.clear();
        self.engine.clear();
    }

    /// Feed an **ascending, sorted** in-memory batch straight into the store — a
    /// dense build. Pure memory: the checkpoint loader in
    /// [`crate::persist::stream`] reads batches off disk and calls this, so the
    /// trie never touches a reader. Cache/dirty are left untouched (empty on a
    /// fresh build; the first `root` recomputes the cache).
    pub(crate) fn bulk_extend_sorted(&mut self, batch: Vec<(Vec<u8>, S::Value)>) {
        self.store.bulk_extend_sorted(batch);
    }

    /// Merkle root of the current store, reusing cached hashes for unchanged
    /// subtrees. Clears the pending dirty set; keeps the cache.
    pub(crate) fn root(&mut self) -> H256 {
        self.engine.root(&self.store)
    }

    /// Number of stored entries (tombstones included).
    pub(crate) fn len(&self) -> usize {
        self.store.len()
    }
}

impl<V: TrieValue> AuthenticatedTrie<Entries<V>> {
    /// An empty **snapshot** trie: `SkipSingleton` cache policy. The snapshot is
    /// the only layer that skips singleton subtree caching.
    pub(crate) fn empty_snapshot() -> Self {
        Self {
            store: Entries::new(),
            engine: RootEngine::new(CachePolicy::SkipSingleton),
        }
    }

    /// Build a **snapshot** trie from a canonical snapshot map (all live values,
    /// no tombstones); `SkipSingleton` policy. The cache starts empty, so the
    /// first `root` is a full recompute that populates it; later (incremental)
    /// merges clone that warm cache.
    pub(crate) fn from_snapshot(snapshot: &BTreeMap<Vec<u8>, Box<[u8]>>) -> Self {
        let mut store = Entries::new();
        for (k, v) in snapshot {
            store.insert(k, V::from_value_bytes(v));
        }
        Self {
            store,
            engine: RootEngine::new(CachePolicy::SkipSingleton),
        }
    }
}

impl AuthenticatedTrie<Entries<MptValue>> {
    /// Build a **delta** trie from an existing byte-keyed delta (e.g. a persisted
    /// state); `Full` policy. The cache starts empty, so the first `root` is a
    /// full (correct) recompute that populates it. Delta-only, so it keeps
    /// `MptValue` (tombstones are meaningful in the delta).
    pub(crate) fn from_delta(delta: &BTreeMap<Vec<u8>, MptValue>) -> Self {
        let mut store = Entries::new();
        for (k, v) in delta {
            store.insert(k, v.clone());
        }
        Self {
            store,
            engine: RootEngine::new(CachePolicy::Full),
        }
    }
}

/// The two-level-backed incremental trie (the historical `IncrementalTrie`).
///
/// A thin newtype over the generic [`AuthenticatedTrie`] combinator pinned to the
/// two-level [`Entries`] store. It exists to keep the store type and its trait
/// bounds (`TrieStore` / `TrieReader`, and their `Nibbles` views) **out of the
/// crate's public surface**: only value-clean signatures (`V`, `H256`, byte
/// slices) cross this boundary, so the `#[cfg(fuzzing)]` export in `lib.rs` needs
/// nothing else made public. Delegates every method to the inner combinator.
///
/// Generic over the stored value type `V` (default [`MptValue`]): the delta and
/// intermediate tries keep `MptValue` (which carries `Tombstone`), while the
/// snapshot — provably tombstone-free — instantiates it with a compact inline
/// value type to save memory. Only [`TrieValue`] (merkle bytes + visibility) is
/// required, so the hashing and cache logic are identical across value types.
#[derive(Clone, Debug)]
pub struct IncrementalTrie<V: TrieValue = MptValue>(AuthenticatedTrie<Entries<V>>);

/// Delta-trie default: `Full` cache policy (see [`AuthenticatedTrie`]'s default).
impl<V: TrieValue> Default for IncrementalTrie<V> {
    fn default() -> Self {
        Self(AuthenticatedTrie::default())
    }
}

impl<V: TrieValue> IncrementalTrie<V> {
    /// Insert / overwrite the value for `key` (canonical byte mpt key).
    pub fn insert(&mut self, key: &[u8], value: V) {
        self.0.insert(key, value);
    }

    /// Remove `key` (canonical byte form).
    pub fn remove(&mut self, key: &[u8]) {
        self.0.remove(key);
    }

    /// Drop all entries, cached hashes and pending dirty keys.
    pub fn clear(&mut self) {
        self.0.clear();
    }

    /// Merkle root, reusing cached hashes for unchanged subtrees.
    pub fn root(&mut self) -> H256 {
        self.0.root()
    }

    /// Number of stored entries (tombstones included).
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Feed an **ascending, sorted** in-memory batch straight into the store.
    pub(crate) fn bulk_extend_sorted(&mut self, batch: Vec<(Vec<u8>, V)>) {
        self.0.bulk_extend_sorted(batch);
    }

    /// Borrow the underlying store for value-typed reads (snapshot layer).
    pub(crate) fn store(&self) -> &Entries<V> {
        self.0.store()
    }

    /// An empty **snapshot** trie (`SkipSingleton` policy).
    pub(crate) fn empty_snapshot() -> Self {
        Self(AuthenticatedTrie::empty_snapshot())
    }

    /// Build a **snapshot** trie from a canonical snapshot map (`SkipSingleton`).
    pub fn from_snapshot(snapshot: &BTreeMap<Vec<u8>, Box<[u8]>>) -> Self {
        Self(AuthenticatedTrie::from_snapshot(snapshot))
    }
}

impl IncrementalTrie<MptValue> {
    /// Build a **delta** trie from an existing byte-keyed delta (`Full` policy).
    pub fn from_delta(delta: &BTreeMap<Vec<u8>, MptValue>) -> Self {
        Self(AuthenticatedTrie::from_delta(delta))
    }
}

/// A **flat-`BTreeMap`-backed** incremental trie: the generic combinator pinned
/// to [`store::flat::FlatStore`] instead of the two-level [`Entries`].
///
/// A value-clean newtype (like [`IncrementalTrie`]) exposed only under `fuzzing`
/// so the `incremental_trie` differential fuzz can drive the same op sequence
/// through **both** stores and assert equal roots — the store-independence the
/// [`reader::TrieReader`] abstraction promises. It has no production use in this
/// commit (the delta layer wires `FlatStore` in a later commit); `Full` cache
/// policy (delta semantics), matching what the differential fuzz needs.
#[cfg(fuzzing)]
pub struct FlatTrie<V: TrieValue = MptValue>(AuthenticatedTrie<store::flat::FlatStore<V>>);

#[cfg(fuzzing)]
impl<V: TrieValue> Default for FlatTrie<V> {
    fn default() -> Self {
        Self(AuthenticatedTrie::default())
    }
}

#[cfg(fuzzing)]
impl<V: TrieValue> FlatTrie<V> {
    /// Insert / overwrite the value for `key` (canonical byte mpt key).
    pub fn insert(&mut self, key: &[u8], value: V) {
        self.0.insert(key, value);
    }

    /// Remove `key` (canonical byte form).
    pub fn remove(&mut self, key: &[u8]) {
        self.0.remove(key);
    }

    /// Merkle root, reusing cached hashes for unchanged subtrees.
    pub fn root(&mut self) -> H256 {
        self.0.root()
    }
}

#[cfg(test)]
mod tests {
    use super::{IncrementalTrie, MptValue};
    use crate::primitives::MERKLE_NULL_NODE;
    use crate::trie::root::PARALLEL_REBUILD_THRESHOLD;
    use crate::trie::trie_root;
    use std::collections::BTreeMap;

    fn val(b: u8) -> MptValue {
        if b == 0 {
            MptValue::Tombstone
        } else {
            MptValue::Some(Box::from([b]))
        }
    }

    /// Drive a map through a random op sequence, asserting the incremental root
    /// equals the stateless oracle after every change.
    #[test]
    fn matches_oracle_under_random_ops() {
        // Small deterministic LCG so the test is reproducible without deps.
        let mut seed: u64 = 0x9e3779b97f4a7c15;
        let mut next = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            (seed >> 33) as u32
        };

        let mut map: BTreeMap<Vec<u8>, MptValue> = BTreeMap::new();
        let mut inc = IncrementalTrie::default();

        for op_i in 0..4000 {
            // Keys share prefixes to exercise path compression split/merge.
            let klen = 1 + (next() % 4) as usize;
            let key: Vec<u8> = (0..klen).map(|_| (next() % 6) as u8).collect();
            let v = (next() % 4) as u8;

            let action = if v == 0 && map.contains_key(&key) {
                map.remove(&key);
                inc.remove(&key);
                "remove"
            } else {
                map.insert(key.clone(), val(v));
                inc.insert(&key, val(v));
                "insert"
            };

            // Mostly batch several ops before comparing (exercising multi-dirty
            // commits), but compare often enough to localise any divergence.
            if next() % 3 == 0 {
                assert_eq!(
                    inc.root(),
                    trie_root(&map),
                    "diverged at op {op_i}: {action} key={key:?} size {}",
                    map.len()
                );
            }
        }
        assert_eq!(inc.root(), trie_root(&map));
    }

    /// Same, but compare after *every* op so a single-op update bug cannot hide
    /// behind a later full recompute.
    #[test]
    fn matches_oracle_every_op() {
        let mut seed: u64 = 0x2545f4914f6cdd1d;
        let mut next = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            (seed >> 33) as u32
        };
        let mut map: BTreeMap<Vec<u8>, MptValue> = BTreeMap::new();
        let mut inc = IncrementalTrie::default();
        for op_i in 0..4000 {
            let klen = 1 + (next() % 5) as usize;
            let key: Vec<u8> = (0..klen).map(|_| (next() % 6) as u8).collect();
            let v = (next() % 4) as u8;
            if v == 0 && map.contains_key(&key) {
                map.remove(&key);
                inc.remove(&key);
            } else {
                map.insert(key.clone(), val(v));
                inc.insert(&key, val(v));
            }
            assert_eq!(
                inc.root(),
                trie_root(&map),
                "diverged at op {op_i} key={key:?}"
            );
        }
    }

    #[test]
    fn empty_after_clear_is_null() {
        let mut inc = IncrementalTrie::default();
        let mut map = BTreeMap::new();
        map.insert(vec![1u8], MptValue::Some(Box::from([1u8])));
        inc.insert(&[1u8], MptValue::Some(Box::from([1u8])));
        assert_eq!(inc.root(), trie_root(&map));
        inc.clear();
        assert_eq!(inc.root(), MERKLE_NULL_NODE);
    }

    /// `from_delta` reproduces the oracle for a preloaded delta.
    #[test]
    fn from_delta_matches_oracle() {
        let mut map: BTreeMap<Vec<u8>, MptValue> = BTreeMap::new();
        for b in 0u8..50 {
            map.insert(vec![b % 7, b % 5, b], val(b | 1));
        }
        let mut inc = IncrementalTrie::from_delta(&map);
        assert_eq!(inc.root(), trie_root(&map));
    }

    /// Force the cold `memo_node_rebuild` PARALLEL fan-out (>= threshold entries,
    /// empty cache, >1 child at the root) and assert it agrees with the stateless
    /// oracle — the small-map tests above stay under the serial threshold.
    #[test]
    fn parallel_rebuild_matches_oracle() {
        let mut map: BTreeMap<Vec<u8>, MptValue> = BTreeMap::new();
        for i in 0u32..8000 {
            // First byte = i&0xff spans all 16 top nibbles (root fans out); the
            // first two bytes are a bijection with i, so keys stay distinct.
            let key = vec![(i & 0xff) as u8, (i >> 8) as u8, ((i >> 3) & 0xff) as u8];
            map.insert(key, val((i as u8) | 1));
        }
        assert!(map.len() >= PARALLEL_REBUILD_THRESHOLD);
        let mut inc = IncrementalTrie::from_delta(&map);
        assert_eq!(inc.root(), trie_root(&map));
    }

    /// Same cold parallel-rebuild path, but with tombstones interleaved. Proves
    /// a terminal tombstone still hashes as `Some(&[])` (the `b'v'` marker in
    /// `compute_node_merkle`) — byte-identical to the stateless oracle. The
    /// Some/None distinction is carried by key-presence (`find` on nibble
    /// length), which the flattening to merkle bytes does not touch;
    /// `merkle_value()` collapses tombstone and empty `Some` to the same `&[]`,
    /// exactly as the oracle does.
    #[test]
    fn parallel_rebuild_with_tombstones_matches_oracle() {
        let mut map: BTreeMap<Vec<u8>, MptValue> = BTreeMap::new();
        for i in 0u32..8000 {
            let key = vec![(i & 0xff) as u8, (i >> 8) as u8, ((i >> 3) & 0xff) as u8];
            // Every 5th entry is a tombstone (some become terminal-value nodes);
            // one real entry is deliberately an empty value to exercise the
            // tombstone-vs-empty-`Some` collapse.
            let v = if i % 5 == 0 {
                MptValue::Tombstone
            } else if i % 5 == 1 {
                MptValue::Some(Box::from([]))
            } else {
                MptValue::Some(Box::from([(i as u8) | 1]))
            };
            map.insert(key, v);
        }
        assert!(map.len() >= PARALLEL_REBUILD_THRESHOLD);
        let mut inc = IncrementalTrie::from_delta(&map);
        assert_eq!(inc.root(), trie_root(&map));
    }

    /// Wide byte-aligned keys (like real 20/57-byte mpt keys) that produce odd
    /// routing depths and long shared prefixes — exercises the packed
    /// `NibblePrefix` range translation end-to-end against the oracle.
    #[test]
    fn wide_byte_keys_match_oracle() {
        let mut seed: u64 = 0xd1b54a32d192ed03;
        let mut next = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            (seed >> 24) as u8
        };
        let mut map: BTreeMap<Vec<u8>, MptValue> = BTreeMap::new();
        let mut inc = IncrementalTrie::default();
        // A shared 20-byte account prefix + varying storage tails → deep nodes.
        let account: Vec<u8> = (0..20).map(|_| next()).collect();
        for _ in 0..3000 {
            let mut key = account.clone();
            key.extend((0..37).map(|_| next()));
            let v = MptValue::Some(Box::from([next(), next()]));
            inc.insert(&key, v.clone());
            map.insert(key, v);
        }
        // Plus some entirely independent keys.
        for _ in 0..500 {
            let key: Vec<u8> = (0..(1 + (next() % 30) as usize)).map(|_| next()).collect();
            let v = MptValue::Some(Box::from([next()]));
            inc.insert(&key, v.clone());
            map.insert(key, v);
        }
        assert_eq!(inc.root(), trie_root(&map));
    }

    /// Micro-benchmark (run: `cargo test --release -p cfx-minimal-mpt \
    /// bench_incremental -- --nocapture --ignored`). Realistic delta of wide
    /// random 40-byte keys; per round modify a few keys then take the root.
    #[test]
    #[ignore]
    fn bench_incremental_vs_oracle() {
        use std::time::Instant;
        fn lcg(s: &mut u64) -> u64 {
            *s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
            *s >> 33
        }
        let mut seed = 0xdead_beef_u64;
        for &n in &[2000usize, 6000] {
            let mut keys: Vec<Vec<u8>> = Vec::with_capacity(n);
            let mut map: BTreeMap<Vec<u8>, MptValue> = BTreeMap::new();
            let mut inc = IncrementalTrie::default();
            for _ in 0..n {
                let key: Vec<u8> = (0..40).map(|_| (lcg(&mut seed) & 0xff) as u8).collect();
                inc.insert(&key, MptValue::Some(Box::from([1u8])));
                map.insert(key.clone(), MptValue::Some(Box::from([1u8])));
                keys.push(key);
            }
            inc.root(); // warm

            let rounds = 300usize;
            let mut s = seed;
            let inc_t = Instant::now();
            for _ in 0..rounds {
                for _ in 0..4 {
                    s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
                    let k = &keys[(s >> 33) as usize % keys.len()];
                    let v = MptValue::Some(Box::from([(s >> 40) as u8]));
                    inc.insert(k, v.clone());
                    map.insert(k.clone(), v);
                }
                let _ = inc.root();
            }
            let inc_dur = inc_t.elapsed();

            let mut s = seed;
            let or_t = Instant::now();
            for _ in 0..rounds {
                for _ in 0..4 {
                    s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
                    let k = &keys[(s >> 33) as usize % keys.len()];
                    map.insert(k.clone(), MptValue::Some(Box::from([(s >> 40) as u8])));
                }
                let _ = trie_root(&map);
            }
            let or_dur = or_t.elapsed();

            eprintln!(
                "N={n} rounds={rounds}: incremental={inc_dur:?} oracle={or_dur:?} \
                 speedup={:.2}x  (per-commit inc={:?} oracle={:?})",
                or_dur.as_secs_f64() / inc_dur.as_secs_f64(),
                inc_dur / rounds as u32,
                or_dur / rounds as u32,
            );
        }
    }
}
