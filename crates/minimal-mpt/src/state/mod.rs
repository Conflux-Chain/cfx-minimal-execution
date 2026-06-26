use crate::{
    incremental::CachePolicy,
    key_codec::{DeltaMptKeyPadding, StorageKeyWithSpace},
    snapshot::SnapshotTrie,
    store::{MptValueDisk, PersistedState, StateStore},
    trie::{trie_root, MptValue},
    types::{CommitRoot, MptKeyValue, Result, H256, MERKLE_NULL_NODE},
};
use std::collections::BTreeMap;

mod prefix;
mod rotation;

pub const DEFAULT_SNAPSHOT_EPOCH_COUNT: u32 = 2000;

/// Env-gated (`MMPT_DELTA_TIMING=1`) accumulator for time spent in the
/// incremental delta-root call, so a replay run can report real per-commit delta
/// cost in isolation from the snapshot merge. Zero overhead when disabled.
static DELTA_ROOT_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static DELTA_ROOT_CNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
fn delta_timing_on() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("MMPT_DELTA_TIMING").is_some())
}

pub trait StateTrait {
    fn get(&self, key: StorageKeyWithSpace) -> Result<Option<Box<[u8]>>>;
    fn set(&mut self, key: StorageKeyWithSpace, value: Box<[u8]>) -> Result<()>;
    fn get_all_by_prefix(&self, prefix: StorageKeyWithSpace) -> Result<Option<Vec<MptKeyValue>>>;
    fn delete_all_by_prefix(
        &mut self,
        prefix: StorageKeyWithSpace,
    ) -> Result<Option<Vec<MptKeyValue>>>;
    fn commit(&mut self) -> Result<CommitRoot>;
}

#[derive(Debug)]
pub struct State {
    /// The snapshot as a single `IncrementalTrie`: it serves snapshot reads and
    /// owns the persistent subtree-hash cache, so the boundary merge re-roots it
    /// incrementally **in place**. Read-only during a period; only
    /// `advance_after_commit` mutates it, at boundaries. No clone, no double
    /// buffer: the incremental root is cheap enough to do synchronously.
    snapshot: SnapshotTrie,
    intermediate: BTreeMap<Vec<u8>, MptValue>,
    delta: BTreeMap<Vec<u8>, MptValue>,
    snapshot_root: H256,
    intermediate_root: H256,
    intermediate_padding: DeltaMptKeyPadding,
    delta_padding: DeltaMptKeyPadding,
    height: u64,
    snapshot_epoch_count: u32,
    last_root: Option<CommitRoot>,
    // Memoized delta-trie root: writers mark dirty keys here so `commit` only
    // re-hashes changed subtrees instead of the whole delta (see
    // `crate::incremental`). Holds no data, just cached subtree hashes.
    delta_inc: crate::incremental::IncrementalTrie,
    /// Per-period caches of `new_account_key` derivations (the hot keccak in
    /// `to_delta_mpt_key_bytes`), one per active padding. `RefCell` so `get` can
    /// fill them behind `&self`; padding-stamped so they self-invalidate, and
    /// rotated at the boundary — `intermediate(k)`'s padding equals `delta(k-1)`'s,
    /// so the old delta cache is exactly the new intermediate one.
    delta_account_cache: std::cell::RefCell<crate::key_codec::AccountKeyCache>,
    intermediate_account_cache: std::cell::RefCell<crate::key_codec::AccountKeyCache>,
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

impl State {
    pub fn new() -> Self {
        Self {
            snapshot: SnapshotTrie::default(),
            intermediate: BTreeMap::new(),
            delta: BTreeMap::new(),
            snapshot_root: MERKLE_NULL_NODE,
            intermediate_root: MERKLE_NULL_NODE,
            intermediate_padding: DeltaMptKeyPadding::genesis(),
            delta_padding: DeltaMptKeyPadding::genesis(),
            height: 0,
            snapshot_epoch_count: DEFAULT_SNAPSHOT_EPOCH_COUNT,
            last_root: None,
            delta_inc: crate::incremental::IncrementalTrie::default(),
            delta_account_cache: Default::default(),
            intermediate_account_cache: Default::default(),
        }
    }

    pub fn with_snapshot_epoch_count(snapshot_epoch_count: u32) -> Self {
        Self {
            snapshot_epoch_count: snapshot_epoch_count.max(1),
            ..Self::new()
        }
    }

    pub fn from_snapshot(snapshot: BTreeMap<Vec<u8>, Box<[u8]>>) -> Self {
        let mut snapshot_inc = SnapshotTrie::from_snapshot(&snapshot);
        let snapshot_root = snapshot_inc.root_with_policy(CachePolicy::SkipSingleton);
        let delta_padding = DeltaMptKeyPadding::from_roots(snapshot_root, MERKLE_NULL_NODE);
        Self {
            snapshot: snapshot_inc,
            snapshot_root,
            delta_padding,
            ..Self::new()
        }
    }

    pub fn from_persisted(state: PersistedState) -> Self {
        let recover_timing = std::env::var_os("MMPT_RECOVER_TIMING").is_some();
        let recover_total = std::time::Instant::now();
        let snapshot_entries = state.snapshot.len();
        let intermediate_entries = state.intermediate.len();
        let delta_entries = state.delta.len();

        let t = std::time::Instant::now();
        let intermediate = state
            .intermediate
            .into_iter()
            .map(|(k, v)| (k, MptValue::from(v)))
            .collect();
        if recover_timing {
            eprintln!(
                "[recover] intermediate_build entries={} elapsed={}ms",
                intermediate_entries,
                t.elapsed().as_millis()
            );
        }
        let t = std::time::Instant::now();
        let delta = state
            .delta
            .into_iter()
            .map(|(k, v)| (k, MptValue::from(v)))
            .collect();
        if recover_timing {
            eprintln!(
                "[recover] delta_build entries={} elapsed={}ms",
                delta_entries,
                t.elapsed().as_millis()
            );
        }
        let t = std::time::Instant::now();
        let mut snapshot_inc = SnapshotTrie::from_snapshot(&state.snapshot);
        if recover_timing {
            eprintln!(
                "[recover] snapshot_build entries={} elapsed={}ms",
                snapshot_entries,
                t.elapsed().as_millis()
            );
        }
        let t = std::time::Instant::now();
        let snapshot_root = snapshot_inc.root_with_policy(CachePolicy::SkipSingleton);
        if recover_timing {
            eprintln!(
                "[recover] snapshot_root entries={} elapsed={}ms",
                snapshot_entries,
                t.elapsed().as_millis()
            );
        }
        let t = std::time::Instant::now();
        let intermediate_root = trie_root(&intermediate);
        if recover_timing {
            eprintln!(
                "[recover] intermediate_root entries={} elapsed={}ms",
                intermediate_entries,
                t.elapsed().as_millis()
            );
        }
        let intermediate_padding = DeltaMptKeyPadding(state.intermediate_mpt_key_padding);
        let delta_padding = DeltaMptKeyPadding(state.delta_mpt_key_padding);
        // Seed the incremental trie with the loaded delta (one nibble pass);
        // built before `delta` is moved into the struct.
        let t = std::time::Instant::now();
        let delta_inc = crate::incremental::IncrementalTrie::from_delta(&delta);
        if recover_timing {
            eprintln!(
                "[recover] delta_inc_build entries={} elapsed={}ms",
                delta_entries,
                t.elapsed().as_millis()
            );
            eprintln!(
                "[recover] from_persisted_total snapshot={} intermediate={} delta={} elapsed={}ms",
                snapshot_entries,
                intermediate_entries,
                delta_entries,
                recover_total.elapsed().as_millis()
            );
        }
        Self {
            snapshot: snapshot_inc,
            intermediate,
            delta,
            snapshot_root,
            intermediate_root,
            intermediate_padding,
            delta_padding,
            last_root: state.last_root,
            height: state.height,
            snapshot_epoch_count: if state.snapshot_epoch_count == 0 {
                DEFAULT_SNAPSHOT_EPOCH_COUNT
            } else {
                state.snapshot_epoch_count
            },
            delta_inc,
            delta_account_cache: Default::default(),
            intermediate_account_cache: Default::default(),
        }
    }

    pub fn persisted(&self) -> PersistedState {
        PersistedState {
            snapshot: self.snapshot.to_canonical_map(),
            intermediate: self
                .intermediate
                .clone()
                .into_iter()
                .map(|(k, v)| (k, MptValueDisk::from(v)))
                .collect(),
            delta: self
                .delta
                .clone()
                .into_iter()
                .map(|(k, v)| (k, MptValueDisk::from(v)))
                .collect(),
            intermediate_mpt_key_padding: self.intermediate_padding.0,
            delta_mpt_key_padding: self.delta_padding.0,
            height: self.height,
            snapshot_epoch_count: self.snapshot_epoch_count,
            last_root: self.last_root.clone(),
        }
    }

    pub fn put_intermediate_raw(&mut self, raw_key: Vec<u8>, value: MptValue) {
        self.intermediate.insert(raw_key, value);
        self.intermediate_root = trie_root(&self.intermediate);
        self.delta_padding =
            DeltaMptKeyPadding::from_roots(self.snapshot_root, self.intermediate_root);
    }

    pub fn height(&self) -> u64 {
        self.height
    }

    pub fn snapshot_epoch_count(&self) -> u32 {
        self.snapshot_epoch_count
    }
}

impl StateTrait for State {
    fn get(&self, key: StorageKeyWithSpace) -> Result<Option<Box<[u8]>>> {
        let delta_key = {
            let mut cache = self.delta_account_cache.borrow_mut();
            key.to_delta_mpt_key_bytes(&self.delta_padding, Some(&mut *cache))?
        };
        if let Some(value) = self.delta.get(&delta_key) {
            return Ok(value.visible_value().map(Box::from));
        }
        let intermediate_key = {
            let mut cache = self.intermediate_account_cache.borrow_mut();
            key.to_delta_mpt_key_bytes(&self.intermediate_padding, Some(&mut *cache))?
        };
        if let Some(value) = self.intermediate.get(&intermediate_key) {
            return Ok(value.visible_value().map(Box::from));
        }
        Ok(self.snapshot.snapshot_get_owned(&key.to_key_bytes()?))
    }

    fn set(&mut self, key: StorageKeyWithSpace, value: Box<[u8]>) -> Result<()> {
        let raw = key.to_delta_mpt_key_bytes(
            &self.delta_padding,
            Some(self.delta_account_cache.get_mut()),
        )?;
        let value = if value.is_empty() {
            MptValue::Tombstone
        } else {
            MptValue::Some(value)
        };
        self.delta_inc.insert(&raw, value.clone());
        self.delta.insert(raw, value);
        Ok(())
    }

    fn get_all_by_prefix(&self, prefix: StorageKeyWithSpace) -> Result<Option<Vec<MptKeyValue>>> {
        let values = self.read_prefix(prefix)?;
        Ok((!values.is_empty()).then_some(values))
    }

    fn delete_all_by_prefix(
        &mut self,
        prefix: StorageKeyWithSpace,
    ) -> Result<Option<Vec<MptKeyValue>>> {
        let values = self.delete_prefix(prefix)?;
        Ok((!values.is_empty()).then_some(values))
    }

    fn commit(&mut self) -> Result<CommitRoot> {
        let delta_root = if delta_timing_on() {
            use std::sync::atomic::Ordering::Relaxed;
            let t = std::time::Instant::now();
            let r = self.delta_inc.root();
            let ns = t.elapsed().as_nanos() as u64;
            let sum = DELTA_ROOT_NS.fetch_add(ns, Relaxed) + ns;
            let cnt = DELTA_ROOT_CNT.fetch_add(1, Relaxed) + 1;
            if cnt.is_multiple_of(20_000) {
                eprintln!(
                    "[delta-root] commits={cnt} avg={}us total={}ms last={}us N={}",
                    sum / cnt / 1000,
                    sum / 1_000_000,
                    ns / 1000,
                    self.delta.len(),
                );
            }
            r
        } else {
            self.delta_inc.root()
        };
        // Cross-check against the stateless oracle, but only under tests / the
        // `verify-incremental` feature (fuzzing). NOT `debug_assert!`: this
        // workspace builds release with `debug-assertions = true`, so a
        // debug_assert here would run the full O(N) `trie_root` every commit in
        // the replay and erase the incremental speedup.
        #[cfg(any(test, feature = "verify-incremental"))]
        assert_eq!(
            delta_root,
            trie_root(&self.delta),
            "incremental delta root diverged from stateless trie_root"
        );
        let root = CommitRoot::new(
            self.snapshot_root,
            self.intermediate_root,
            delta_root,
            self.delta_padding.0,
        );
        self.last_root = Some(root.clone());
        self.height += 1;
        self.advance_after_commit(delta_root)?;
        Ok(root)
    }
}

pub struct StateManager<S: StateStore> {
    state: State,
    store: S,
}

impl<S: StateStore> StateManager<S> {
    pub fn new(store: S) -> Result<Self> {
        let state = store
            .load_latest()?
            .map(State::from_persisted)
            .unwrap_or_default();
        Ok(Self { state, store })
    }

    pub fn with_snapshot_epoch_count(store: S, snapshot_epoch_count: u32) -> Result<Self> {
        let mut manager = Self::new(store)?;
        manager.state.snapshot_epoch_count = snapshot_epoch_count.max(1);
        Ok(manager)
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    pub fn state_mut(&mut self) -> &mut State {
        &mut self.state
    }

    pub fn commit(&mut self) -> Result<CommitRoot> {
        let root = self.state.commit()?;
        self.store.save_latest(&self.state.persisted())?;
        Ok(root)
    }
}

impl<S: StateStore> StateTrait for StateManager<S> {
    fn get(&self, key: StorageKeyWithSpace) -> Result<Option<Box<[u8]>>> {
        self.state.get(key)
    }

    fn set(&mut self, key: StorageKeyWithSpace, value: Box<[u8]>) -> Result<()> {
        self.state.set(key, value)
    }

    fn get_all_by_prefix(&self, prefix: StorageKeyWithSpace) -> Result<Option<Vec<MptKeyValue>>> {
        self.state.get_all_by_prefix(prefix)
    }

    fn delete_all_by_prefix(
        &mut self,
        prefix: StorageKeyWithSpace,
    ) -> Result<Option<Vec<MptKeyValue>>> {
        self.state.delete_all_by_prefix(prefix)
    }

    fn commit(&mut self) -> Result<CommitRoot> {
        StateManager::commit(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileStore, MemoryStore, Space, StorageKey};
    use std::fs;

    fn key(byte: u8) -> StorageKeyWithSpace {
        StorageKeyWithSpace {
            key: StorageKey::Account(vec![byte; 20]),
            space: Space::Native,
        }
    }

    #[test]
    fn get_set_commit() {
        let mut state = State::new();
        state.set(key(1), Box::from([9u8])).unwrap();
        assert_eq!(state.get(key(1)).unwrap().unwrap().as_ref(), &[9u8]);
        let root = state.commit().unwrap();
        assert_ne!(root.delta_root, MERKLE_NULL_NODE);
    }

    #[test]
    fn tombstone_hides_value() {
        let mut snapshot = BTreeMap::new();
        snapshot.insert(key(1).to_key_bytes().unwrap(), Box::from([1u8]));
        let mut state = State::from_snapshot(snapshot);
        assert_eq!(state.get(key(1)).unwrap().unwrap().as_ref(), &[1u8]);
        state.set(key(1), Box::new([])).unwrap();
        assert!(state.get(key(1)).unwrap().is_none());
    }

    #[test]
    fn address_prefix_reads_delta_after_canonical_filter() {
        let mut state = State::new();
        state.set(key(1), Box::from([1u8])).unwrap();
        state.set(key(2), Box::from([2u8])).unwrap();
        let prefix = StorageKeyWithSpace {
            key: StorageKey::AddressPrefix(vec![1]),
            space: Space::Native,
        };
        let values = state.get_all_by_prefix(prefix).unwrap().unwrap();
        assert_eq!(values.len(), 1);
        assert_eq!(values[0].0, key(1).to_key_bytes().unwrap());
    }

    #[test]
    fn address_prefix_delete_removes_unrelated_delta_keys() {
        let mut state = State::new();
        state.set(key(1), Box::from([1u8])).unwrap();
        state.set(key(2), Box::from([2u8])).unwrap();
        let prefix = StorageKeyWithSpace {
            key: StorageKey::AddressPrefix(vec![1]),
            space: Space::Native,
        };
        let values = state.delete_all_by_prefix(prefix).unwrap().unwrap();
        assert_eq!(values.len(), 1);
        assert!(state.get(key(1)).unwrap().is_none());
        assert!(state.get(key(2)).unwrap().is_none());
    }

    #[test]
    fn delete_all_prefix_hides_snapshot_values() {
        let mut snapshot = BTreeMap::new();
        snapshot.insert(key(1).to_key_bytes().unwrap(), Box::from([1u8]));
        let mut state = State::from_snapshot(snapshot);
        let deleted = state
            .delete_all_by_prefix(StorageKeyWithSpace {
                key: StorageKey::AddressPrefix(vec![1]),
                space: Space::Native,
            })
            .unwrap()
            .unwrap();
        assert_eq!(deleted.len(), 1);
        assert!(state.get(key(1)).unwrap().is_none());
    }

    #[test]
    fn manager_persists_latest_state() {
        let path =
            std::env::temp_dir().join(format!("cfx-minimal-mpt-test-{}.bin", std::process::id()));
        let _ = fs::remove_file(&path);

        {
            let mut manager = StateManager::new(FileStore::new(&path)).unwrap();
            manager.set(key(2), Box::from([7u8])).unwrap();
            manager.commit().unwrap();
        }

        let manager = StateManager::new(FileStore::new(&path)).unwrap();
        assert_eq!(manager.get(key(2)).unwrap().unwrap().as_ref(), &[7u8]);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn memory_store_manager_uses_latest_only() {
        let mut manager = StateManager::new(MemoryStore::new()).unwrap();
        manager.set(key(3), Box::from([8u8])).unwrap();
        let root = manager.commit().unwrap();
        assert_ne!(root.delta_root, MERKLE_NULL_NODE);
    }

    #[test]
    fn deterministic_random_set_get_commit_sequence() {
        let mut rng = Lcg::new(0x13_57_9b_df);
        let mut state = State::new();
        let mut expected = BTreeMap::<u8, Option<Vec<u8>>>::new();

        for i in 0..5_000 {
            let id = (rng.next() % 96) as u8;
            match rng.next() % 5 {
                0 => {
                    state.set(key(id), Box::new([])).unwrap();
                    expected.insert(id, None);
                }
                1..=3 => {
                    let value = vec![id, i as u8, (i >> 8) as u8];
                    state
                        .set(key(id), value.clone().into_boxed_slice())
                        .unwrap();
                    expected.insert(id, Some(value));
                }
                _ => {
                    let actual = state.get(key(id)).unwrap().map(|v| v.to_vec());
                    let expected_value = expected.get(&id).cloned().flatten();
                    assert_eq!(actual, expected_value, "id={id}, i={i}");
                }
            }
            if i % 97 == 0 {
                let root_a = state.commit().unwrap();
                let root_b = state.commit().unwrap();
                assert_eq!(root_a, root_b);
            }
        }
    }

    #[derive(Clone)]
    struct Lcg(u64);

    impl Lcg {
        fn new(seed: u64) -> Self {
            Self(seed)
        }

        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
            self.0
        }
    }
}
