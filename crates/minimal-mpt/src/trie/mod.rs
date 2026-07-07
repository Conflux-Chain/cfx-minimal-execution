pub(crate) mod nibbles;
pub(crate) mod reader;
pub(crate) mod root;
pub(crate) mod store;
mod value;

pub use value::MptValue;
pub(crate) use value::{snapshot_value, SnapshotValue};

pub use crate::merkle::oracle::trie_root;
#[cfg(feature = "verify-incremental")]
pub(crate) use crate::merkle::oracle::{bytes_to_nibbles, root_for_entries};
pub(crate) use crate::merkle::rules::{compute_node_merkle, compute_path_merkle, keccak, CHILDREN};

use crate::primitives::H256;
use reader::TrieStore;
use root::{CachePolicy, RootEngine};
use std::collections::BTreeMap;
use store::two_level::TwoLevelStore;

/// A store composed with an incremental [`RootEngine`], fixing the invariant that
/// **every write marks the engine dirty**: `insert` / `remove` update both the
/// `store` and the engine's pending set, so `root` only re-hashes changed
/// subtrees. The engine's [`CachePolicy`] is fixed when the trie is built (delta
/// layers use `Full`, the snapshot uses `SkipSingleton`); it never varies per
/// call. The engine sees the store only through [`TrieReader`] (merkle bytes), so
/// it never names the store's value type.
///
/// Generic over the store `S`: the two-level snapshot [`TwoLevelStore`] or the flat
/// delta [`store::btree::BTreeStore`]. The state layers pin `S` — the snapshot
/// layer wraps `AuthenticatedTrie<TwoLevelStore>`, the delta layer
/// `AuthenticatedTrie<BTreeStore>` — and only their value-clean wrappers cross
/// the crate's public (fuzzing) surface.
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
/// relies on this default (see `AuthenticatedTrie::<TwoLevelStore>::empty_snapshot`).
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
    // Exercised only by the `empty_after_clear_is_null` test; reads dead in the
    // lib build alone.
    #[allow(dead_code)]
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

impl AuthenticatedTrie<TwoLevelStore> {
    /// An empty **snapshot** trie: `SkipSingleton` cache policy. The snapshot is
    /// the only layer that skips singleton subtree caching.
    pub(crate) fn empty_snapshot() -> Self {
        Self {
            store: TwoLevelStore::new(),
            engine: RootEngine::new(CachePolicy::SkipSingleton),
        }
    }

    /// Build a **snapshot** trie from a canonical snapshot map (all live values,
    /// no tombstones); `SkipSingleton` policy. The cache starts empty, so the
    /// first `root` is a full recompute that populates it; later (incremental)
    /// merges clone that warm cache.
    pub(crate) fn from_snapshot(snapshot: &BTreeMap<Vec<u8>, Box<[u8]>>) -> Self {
        let mut store = TwoLevelStore::new();
        for (k, v) in snapshot {
            store.insert(k, snapshot_value(v));
        }
        Self {
            store,
            engine: RootEngine::new(CachePolicy::SkipSingleton),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{snapshot_value, AuthenticatedTrie, MptValue, TwoLevelStore};
    use crate::primitives::MERKLE_NULL_NODE;
    use crate::trie::root::PARALLEL_REBUILD_THRESHOLD;
    use crate::trie::trie_root;
    use std::collections::BTreeMap;

    /// Live value bytes from a tag byte. `0` yields the empty value — legal in
    /// the snapshot, and it hashes exactly like a tombstone (`merkle_value` of
    /// both is `&[]`), keeping that collapse covered here; the tombstone variant
    /// itself is delta-only and covered by the flat store's tests.
    fn val(b: u8) -> Vec<u8> {
        if b == 0 {
            Vec::new()
        } else {
            vec![b]
        }
    }

    fn oracle_val(b: u8) -> MptValue {
        MptValue::Some(val(b).into_boxed_slice())
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
        let mut inc = AuthenticatedTrie::<TwoLevelStore>::empty_snapshot();

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
                map.insert(key.clone(), oracle_val(v));
                inc.insert(&key, snapshot_value(&val(v)));
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
        let mut inc = AuthenticatedTrie::<TwoLevelStore>::empty_snapshot();
        for op_i in 0..4000 {
            let klen = 1 + (next() % 5) as usize;
            let key: Vec<u8> = (0..klen).map(|_| (next() % 6) as u8).collect();
            let v = (next() % 4) as u8;
            if v == 0 && map.contains_key(&key) {
                map.remove(&key);
                inc.remove(&key);
            } else {
                map.insert(key.clone(), oracle_val(v));
                inc.insert(&key, snapshot_value(&val(v)));
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
        let mut inc = AuthenticatedTrie::<TwoLevelStore>::empty_snapshot();
        let mut map = BTreeMap::new();
        map.insert(vec![1u8], MptValue::Some(Box::from([1u8])));
        inc.insert(&[1u8], snapshot_value(&[1u8]));
        assert_eq!(inc.root(), trie_root(&map));
        inc.clear();
        assert_eq!(inc.root(), MERKLE_NULL_NODE);
    }

    /// Force the cold `memo_node_rebuild` PARALLEL fan-out (>= threshold entries,
    /// empty cache via `from_snapshot`, >1 child at the root) and assert it
    /// agrees with the stateless oracle — the small-map tests above stay under
    /// the serial threshold. (The same cold path with tombstones interleaved is
    /// covered by the flat store's tests — the snapshot store cannot hold one.)
    #[test]
    fn parallel_rebuild_matches_oracle() {
        let mut snapshot: BTreeMap<Vec<u8>, Box<[u8]>> = BTreeMap::new();
        let mut map: BTreeMap<Vec<u8>, MptValue> = BTreeMap::new();
        for i in 0u32..8000 {
            // First byte = i&0xff spans all 16 top nibbles (root fans out); the
            // first two bytes are a bijection with i, so keys stay distinct.
            let key = vec![(i & 0xff) as u8, (i >> 8) as u8, ((i >> 3) & 0xff) as u8];
            snapshot.insert(key.clone(), val((i as u8) | 1).into_boxed_slice());
            map.insert(key, oracle_val((i as u8) | 1));
        }
        assert!(map.len() >= PARALLEL_REBUILD_THRESHOLD);
        let mut inc = AuthenticatedTrie::from_snapshot(&snapshot);
        assert_eq!(inc.root(), trie_root(&map));
    }

    /// Wide byte-aligned keys (like real 20/57-byte mpt keys) that produce odd
    /// routing depths and long shared prefixes — exercises the packed
    /// `PrefixKey` range translation end-to-end against the oracle.
    #[test]
    fn wide_byte_keys_match_oracle() {
        let mut seed: u64 = 0xd1b54a32d192ed03;
        let mut next = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            (seed >> 24) as u8
        };
        let mut map: BTreeMap<Vec<u8>, MptValue> = BTreeMap::new();
        let mut inc = AuthenticatedTrie::<TwoLevelStore>::empty_snapshot();
        // A shared 20-byte account prefix + varying storage tails → deep nodes.
        let account: Vec<u8> = (0..20).map(|_| next()).collect();
        for _ in 0..3000 {
            let mut key = account.clone();
            key.extend((0..37).map(|_| next()));
            let v = [next(), next()];
            inc.insert(&key, snapshot_value(&v));
            map.insert(key, MptValue::Some(Box::from(v)));
        }
        // Plus some entirely independent keys.
        for _ in 0..500 {
            let key: Vec<u8> = (0..(1 + (next() % 30) as usize)).map(|_| next()).collect();
            let v = [next()];
            inc.insert(&key, snapshot_value(&v));
            map.insert(key, MptValue::Some(Box::from(v)));
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
            let mut inc = AuthenticatedTrie::<TwoLevelStore>::empty_snapshot();
            for _ in 0..n {
                let key: Vec<u8> = (0..40).map(|_| (lcg(&mut seed) & 0xff) as u8).collect();
                inc.insert(&key, snapshot_value(&[1u8]));
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
                    let v = [(s >> 40) as u8];
                    inc.insert(k, snapshot_value(&v));
                    map.insert(k.clone(), MptValue::Some(Box::from(v)));
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
