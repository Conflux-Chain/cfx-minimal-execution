use crate::{incremental::CachePolicy, trie::MptValue, types::H256};
use std::collections::BTreeMap;

/// The snapshot's value type. The snapshot never stores a tombstone (deletes are
/// applied as removals — see [`SnapshotTrie::apply_updates`]), so it drops
/// `MptValue`'s tombstone variant and inlines values up to 8 bytes: the ~61% of
/// snapshot values that are ≤8 B avoid a heap allocation, trading +8 B of inline
/// `BTreeMap` slot for the saved allocation. Merkle bytes are byte-identical to
/// the old `MptValue::Some`, so the snapshot root and checkpoint format are
/// unchanged.
#[cfg(not(feature = "snapshot-lmdb"))]
pub(crate) type SnapshotValue = smallvec::SmallVec<[u8; 8]>;

#[cfg(not(feature = "snapshot-lmdb"))]
impl crate::trie::TrieValue for SnapshotValue {
    fn merkle_value(&self) -> &[u8] {
        self.as_slice()
    }
    fn visible_value(&self) -> Option<&[u8]> {
        Some(self.as_slice())
    }
    fn from_value_bytes(bytes: &[u8]) -> Self {
        smallvec::SmallVec::from_slice(bytes)
    }
}

#[cfg(not(feature = "snapshot-lmdb"))]
#[derive(Clone, Debug, Default)]
pub(crate) struct SnapshotTrie {
    inner: crate::incremental::IncrementalTrie<SnapshotValue>,
}

#[cfg(not(feature = "snapshot-lmdb"))]
impl SnapshotTrie {
    pub(crate) fn from_snapshot(snapshot: &BTreeMap<Vec<u8>, Box<[u8]>>) -> Self {
        Self {
            inner: crate::incremental::IncrementalTrie::from_snapshot(snapshot),
        }
    }

    /// Stream the snapshot trie from a checkpoint reader, converting keys to
    /// nibbles on the fly — no byte-keyed `BTreeMap` is ever built.
    pub(crate) fn from_reader(reader: &mut impl std::io::Read) -> std::io::Result<Self> {
        Ok(Self {
            inner: crate::incremental::IncrementalTrie::from_snapshot_reader(reader)?,
        })
    }

    pub(crate) fn insert(&mut self, key: &[u8], value: MptValue) {
        // The snapshot is tombstone-free: a delete is a removal, never a stored
        // tombstone. Convert the (always-`Some`) value into the inline type.
        match value {
            MptValue::Some(bytes) => self
                .inner
                .insert(key, <SnapshotValue as crate::trie::TrieValue>::from_value_bytes(&bytes)),
            MptValue::Tombstone => self.inner.remove(key),
        }
    }

    pub(crate) fn remove(&mut self, key: &[u8]) {
        self.inner.remove(key);
    }

    #[cfg(feature = "mem-survey")]
    pub(crate) fn memory_survey(&self) -> crate::incremental::MemorySurvey {
        self.inner.memory_survey()
    }

    pub(crate) fn apply_updates(&mut self, updates: Vec<(Vec<u8>, MptValue)>) {
        for (key, value) in updates {
            match value {
                MptValue::Some(value) => self.insert(&key, MptValue::Some(value)),
                MptValue::Tombstone => self.remove(&key),
            }
        }
    }

    pub(crate) fn root_with_policy(&mut self, cache_policy: CachePolicy) -> H256 {
        self.inner.root_with_policy(cache_policy)
    }

    pub(crate) fn snapshot_get_owned(&self, canonical: &[u8]) -> Option<Box<[u8]>> {
        self.inner.snapshot_get(canonical).map(Box::from)
    }

    pub(crate) fn snapshot_scan_prefix(
        &self,
        canonical_prefix: &[u8],
    ) -> Vec<(Vec<u8>, Box<[u8]>)> {
        self.inner.snapshot_scan_prefix(canonical_prefix)
    }

    pub(crate) fn to_canonical_map(&self) -> BTreeMap<Vec<u8>, Box<[u8]>> {
        self.inner.to_canonical_map()
    }

    pub(crate) fn live_entry_count(&self) -> usize {
        self.inner.live_entry_count()
    }

    pub(crate) fn for_each_canonical(
        &self,
        mut f: impl FnMut(Vec<u8>, &[u8]) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        for (key, value) in self.inner.iter_canonical() {
            f(key, value)?;
        }
        Ok(())
    }

    pub(crate) fn len(&self) -> usize {
        self.inner.len()
    }
}

#[cfg(feature = "snapshot-lmdb")]
mod lmdb {
    use super::*;
    use crate::{
        incremental::PARALLEL_REBUILD_THRESHOLD,
        trie::{bytes_to_nibbles, compute_node_merkle, compute_path_merkle, CHILDREN},
        types::MERKLE_NULL_NODE,
    };
    use heed::{types::Bytes, Database, Env, EnvOpenOptions};
    use rayon::prelude::*;
    use rustc_hash::FxHashMap;
    use std::{collections::BTreeSet, fmt, io};
    use tempfile::TempDir;

    type Cache = FxHashMap<Vec<u8>, H256>;

    pub(crate) struct SnapshotTrie {
        _dir: TempDir,
        env: Env,
        db: Database<Bytes, Bytes>,
        cache: Cache,
        dirty: BTreeSet<Vec<u8>>,
        len: usize,
    }

    impl fmt::Debug for SnapshotTrie {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("SnapshotTrie")
                .field("backend", &"lmdb")
                .field("len", &self.len)
                .field("cache_len", &self.cache.len())
                .field("dirty_len", &self.dirty.len())
                .finish()
        }
    }

    impl Default for SnapshotTrie {
        fn default() -> Self {
            Self::from_snapshot(&BTreeMap::new())
        }
    }

    impl SnapshotTrie {
        pub(crate) fn from_snapshot(snapshot: &BTreeMap<Vec<u8>, Box<[u8]>>) -> Self {
            let dir = tempfile::Builder::new()
                .prefix("cfx-mmpt-snapshot-lmdb-")
                .tempdir()
                .expect("create temporary LMDB snapshot directory");
            let env = unsafe {
                EnvOpenOptions::new()
                    .map_size(lmdb_map_size())
                    .max_dbs(1)
                    .open(dir.path())
                    .expect("open temporary LMDB snapshot")
            };
            let mut wtxn = env.write_txn().expect("open LMDB snapshot write txn");
            let db = env
                .create_database(&mut wtxn, Some("snapshot"))
                .expect("create LMDB snapshot database");
            for (key, value) in snapshot {
                let nibbles = bytes_to_nibbles(key);
                db.put(&mut wtxn, nibbles.as_slice(), value.as_ref())
                    .expect("load snapshot entry into LMDB");
            }
            wtxn.commit().expect("commit LMDB snapshot load");
            let (_, cache) = rebuild_snapshot_root_cache(snapshot, CachePolicy::SkipSingleton);
            Self {
                _dir: dir,
                env,
                db,
                cache,
                dirty: BTreeSet::new(),
                len: snapshot.len(),
            }
        }

        /// Stream the snapshot from a checkpoint reader straight into LMDB,
        /// never materializing the byte-keyed `BTreeMap`. Each entry is written
        /// to LMDB and simultaneously pushed into an owned nibble-entry vector,
        /// so the parallel hash-cache rebuild runs off that vector without a
        /// second full pass over the snapshot. The owned vector (one transient
        /// copy of the snapshot) is dropped as soon as the cache is built.
        pub(crate) fn from_reader(reader: &mut impl io::Read) -> io::Result<Self> {
            use crate::codec::{read_len_prefixed, read_u64_le};
            let count = read_u64_le(reader)? as usize;
            let dir = tempfile::Builder::new()
                .prefix("cfx-mmpt-snapshot-lmdb-")
                .tempdir()
                .map_err(lmdb_io_error)?;
            let env = unsafe {
                EnvOpenOptions::new()
                    .map_size(lmdb_map_size())
                    .max_dbs(1)
                    .open(dir.path())
                    .map_err(lmdb_io_error)?
            };
            let mut wtxn = env.write_txn().map_err(lmdb_io_error)?;
            let db = env
                .create_database(&mut wtxn, Some("snapshot"))
                .map_err(lmdb_io_error)?;
            let mut owned: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(count);
            for _ in 0..count {
                let key = read_len_prefixed(reader)?;
                let value = read_len_prefixed(reader)?;
                let nibbles = bytes_to_nibbles(&key);
                db.put(&mut wtxn, nibbles.as_slice(), value.as_slice())
                    .map_err(lmdb_io_error)?;
                owned.push((nibbles, value));
            }
            wtxn.commit().map_err(lmdb_io_error)?;
            let (_, cache) =
                rebuild_snapshot_root_cache_from_nibble_entries(&owned, CachePolicy::SkipSingleton);
            Ok(Self {
                _dir: dir,
                env,
                db,
                cache,
                dirty: BTreeSet::new(),
                len: count,
            })
        }

        pub(crate) fn insert(&mut self, key: &[u8], value: MptValue) {
            match value {
                MptValue::Some(value) => {
                    let nibbles = bytes_to_nibbles(key);
                    let mut wtxn = self.env.write_txn().expect("open LMDB snapshot write txn");
                    let existed = self
                        .db
                        .get(&wtxn, &nibbles)
                        .expect("read LMDB snapshot entry before insert")
                        .is_some();
                    self.db
                        .put(&mut wtxn, nibbles.as_slice(), value.as_ref())
                        .expect("insert LMDB snapshot entry");
                    wtxn.commit().expect("commit LMDB snapshot insert");
                    if !existed {
                        self.len += 1;
                    }
                    self.dirty.insert(nibbles);
                }
                MptValue::Tombstone => self.remove(key),
            }
        }

        pub(crate) fn remove(&mut self, key: &[u8]) {
            let nibbles = bytes_to_nibbles(key);
            let mut wtxn = self.env.write_txn().expect("open LMDB snapshot write txn");
            let removed = self
                .db
                .delete(&mut wtxn, &nibbles)
                .expect("delete LMDB snapshot entry");
            wtxn.commit().expect("commit LMDB snapshot delete");
            if removed {
                self.len -= 1;
            }
            self.dirty.insert(nibbles);
        }

        pub(crate) fn apply_updates(&mut self, updates: Vec<(Vec<u8>, MptValue)>) {
            let mut wtxn = self.env.write_txn().expect("open LMDB snapshot write txn");
            for (key, value) in updates {
                let nibbles = bytes_to_nibbles(&key);
                match value {
                    MptValue::Some(value) => {
                        let existed = self
                            .db
                            .get(&wtxn, nibbles.as_slice())
                            .expect("read LMDB snapshot entry before batch insert")
                            .is_some();
                        self.db
                            .put(&mut wtxn, nibbles.as_slice(), value.as_ref())
                            .expect("batch insert LMDB snapshot entry");
                        if !existed {
                            self.len += 1;
                        }
                    }
                    MptValue::Tombstone => {
                        let removed = self
                            .db
                            .delete(&mut wtxn, nibbles.as_slice())
                            .expect("batch delete LMDB snapshot entry");
                        if removed {
                            self.len -= 1;
                        }
                    }
                }
                self.dirty.insert(nibbles);
            }
            wtxn.commit().expect("commit LMDB snapshot batch");
        }

        pub(crate) fn root_with_policy(&mut self, cache_policy: CachePolicy) -> H256 {
            if self.len == 0 {
                self.cache.clear();
                self.dirty.clear();
                return MERKLE_NULL_NODE;
            }
            for nibbles in &self.dirty {
                for len in 0..=nibbles.len() {
                    self.cache.remove(&nibbles[0..len]);
                }
            }
            self.dirty.clear();

            let rtxn = self.env.read_txn().expect("open LMDB snapshot read txn");
            memo_node_lmdb(&rtxn, self.db, &[], false, &mut self.cache, cache_policy)
        }

        pub(crate) fn snapshot_get_owned(&self, canonical: &[u8]) -> Option<Box<[u8]>> {
            let nibbles = bytes_to_nibbles(canonical);
            let rtxn = self.env.read_txn().expect("open LMDB snapshot read txn");
            self.db
                .get(&rtxn, &nibbles)
                .expect("read LMDB snapshot entry")
                .map(Box::from)
        }

        pub(crate) fn snapshot_scan_prefix(
            &self,
            canonical_prefix: &[u8],
        ) -> Vec<(Vec<u8>, Box<[u8]>)> {
            let nibble_prefix = bytes_to_nibbles(canonical_prefix);
            let rtxn = self.env.read_txn().expect("open LMDB snapshot read txn");
            if nibble_prefix.is_empty() {
                self.db
                    .iter(&rtxn)
                    .expect("open LMDB snapshot cursor")
                    .map(|item| {
                        let (key, value) = item.expect("read LMDB snapshot cursor entry");
                        (nibbles_to_bytes(key), Box::from(value))
                    })
                    .collect()
            } else {
                self.db
                    .prefix_iter(&rtxn, nibble_prefix.as_slice())
                    .expect("open LMDB snapshot prefix range")
                    .map(|item| {
                        let (key, value) = item.expect("read LMDB snapshot prefix entry");
                        (nibbles_to_bytes(key), Box::from(value))
                    })
                    .collect()
            }
        }

        pub(crate) fn to_canonical_map(&self) -> BTreeMap<Vec<u8>, Box<[u8]>> {
            let rtxn = self.env.read_txn().expect("open LMDB snapshot read txn");
            self.db
                .iter(&rtxn)
                .expect("open LMDB snapshot cursor")
                .map(|item| {
                    let (key, value) = item.expect("read LMDB snapshot cursor entry");
                    (nibbles_to_bytes(key), Box::from(value))
                })
                .collect()
        }

        pub(crate) fn live_entry_count(&self) -> usize {
            self.len
        }

        pub(crate) fn for_each_canonical(
            &self,
            mut f: impl FnMut(Vec<u8>, &[u8]) -> io::Result<()>,
        ) -> io::Result<()> {
            let rtxn = self.env.read_txn().map_err(lmdb_io_error)?;
            for item in self.db.iter(&rtxn).map_err(lmdb_io_error)? {
                let (key, value) = item.map_err(lmdb_io_error)?;
                f(nibbles_to_bytes(key), value)?;
            }
            Ok(())
        }

        pub(crate) fn len(&self) -> usize {
            self.len
        }
    }

    fn lmdb_map_size() -> usize {
        std::env::var("MMPT_LMDB_MAP_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(64 * 1024 * 1024 * 1024)
    }

    fn lmdb_io_error<E: std::fmt::Display>(e: E) -> io::Error {
        io::Error::new(io::ErrorKind::Other, e.to_string())
    }

    fn first_with_prefix(
        rtxn: &heed::RoTxn<'_>,
        db: Database<Bytes, Bytes>,
        prefix: &[u8],
    ) -> Option<Vec<u8>> {
        if prefix.is_empty() {
            let mut iter = db
                .iter(rtxn)
                .map_err(lmdb_io_error)
                .expect("open LMDB snapshot cursor");
            iter.next()
                .transpose()
                .map_err(lmdb_io_error)
                .expect("read LMDB snapshot cursor")
                .map(|(key, _)| key.to_vec())
        } else {
            let mut iter = db
                .prefix_iter(rtxn, prefix)
                .map_err(lmdb_io_error)
                .expect("open LMDB snapshot prefix cursor");
            iter.next()
                .transpose()
                .map_err(lmdb_io_error)
                .expect("read LMDB snapshot prefix cursor")
                .map(|(key, _)| key.to_vec())
        }
    }

    fn last_with_prefix(
        rtxn: &heed::RoTxn<'_>,
        db: Database<Bytes, Bytes>,
        prefix: &[u8],
    ) -> Option<Vec<u8>> {
        if prefix.is_empty() {
            let mut iter = db
                .rev_iter(rtxn)
                .map_err(lmdb_io_error)
                .expect("open LMDB snapshot reverse cursor");
            iter.next()
                .transpose()
                .map_err(lmdb_io_error)
                .expect("read LMDB snapshot reverse cursor")
                .map(|(key, _)| key.to_vec())
        } else {
            let mut iter = db
                .rev_prefix_iter(rtxn, prefix)
                .map_err(lmdb_io_error)
                .expect("open LMDB snapshot reverse prefix cursor");
            iter.next()
                .transpose()
                .map_err(lmdb_io_error)
                .expect("read LMDB snapshot reverse prefix cursor")
                .map(|(key, _)| key.to_vec())
        }
    }

    fn prefix_has_key(rtxn: &heed::RoTxn<'_>, db: Database<Bytes, Bytes>, prefix: &[u8]) -> bool {
        first_with_prefix(rtxn, db, prefix).is_some()
    }

    fn memo_node_lmdb(
        rtxn: &heed::RoTxn<'_>,
        db: Database<Bytes, Bytes>,
        rp: &[u8],
        allow_path_compression: bool,
        cache: &mut Cache,
        cache_policy: CachePolicy,
    ) -> H256 {
        if let Some(hash) = cache.get(rp) {
            return *hash;
        }
        let depth = rp.len();
        let first_key = first_with_prefix(rtxn, db, rp).expect("non-empty LMDB snapshot node");
        let last_key = if allow_path_compression || cache_policy == CachePolicy::SkipSingleton {
            last_with_prefix(rtxn, db, rp).expect("non-empty LMDB snapshot node")
        } else {
            first_key.clone()
        };
        let is_singleton = first_key == last_key;
        let common = if allow_path_compression {
            common_prefix_len(&first_key, &last_key, depth)
        } else {
            0
        };
        let node_depth = depth + common;
        let full_prefix = &first_key[0..node_depth];
        let maybe_value = if full_prefix.is_empty() {
            None
        } else {
            db.get(rtxn, full_prefix)
                .expect("read LMDB snapshot node value")
        };

        let mut children: [H256; CHILDREN] = [MERKLE_NULL_NODE; CHILDREN];
        for (c, slot) in children.iter_mut().enumerate() {
            let mut child_rp = Vec::with_capacity(node_depth + 1);
            child_rp.extend_from_slice(full_prefix);
            child_rp.push(c as u8);
            if let Some(hash) = cache.get(&child_rp) {
                *slot = *hash;
                continue;
            }
            if prefix_has_key(rtxn, db, &child_rp) {
                *slot = memo_node_lmdb(rtxn, db, &child_rp, true, cache, cache_policy);
            }
        }

        let has_children = children.iter().any(|h| *h != MERKLE_NULL_NODE);
        let node_hash = compute_node_merkle(has_children.then_some(&children), maybe_value);
        let hash = compute_path_merkle(&first_key[depth..node_depth], depth, node_hash);
        if cache_policy == CachePolicy::Full || !is_singleton {
            cache.insert(rp.to_vec(), hash);
        }
        hash
    }

    // --- Nibble-keyed hashing helpers ---------------------------------------
    //
    // The LMDB backend keeps the historical one-nibble-per-byte key layout (its
    // keys live in LMDB, not in RAM, so the packing win doesn't apply). These
    // are the pre-packing versions of the rebuild machinery, kept local so the
    // in-memory backend's `incremental` module can move to packed `NibblePrefix`
    // keys without disturbing this path.

    fn nibbles_to_bytes(nibbles: &[u8]) -> Vec<u8> {
        nibbles.chunks_exact(2).map(|p| (p[0] << 4) | p[1]).collect()
    }

    /// Longest common nibble prefix of two sorted-block endpoints beyond `depth`.
    fn common_prefix_len(first: &[u8], last: &[u8], depth: usize) -> usize {
        first
            .iter()
            .zip(last)
            .skip(depth)
            .take_while(|(a, b)| a == b)
            .count()
    }

    fn child_prefix(full_prefix: &[u8], c: usize) -> Vec<u8> {
        let mut rp = Vec::with_capacity(full_prefix.len() + 1);
        rp.extend_from_slice(full_prefix);
        rp.push(c as u8);
        rp
    }

    /// A nibble-keyed entry fed to [`memo_node_rebuild`].
    #[derive(Clone, Copy)]
    struct EntryRef<'a> {
        key: &'a [u8],
        value: &'a [u8],
    }

    fn child_ranges(entries: &[EntryRef<'_>], node_depth: usize) -> Vec<(usize, usize, usize)> {
        entries
            .iter()
            .enumerate()
            .filter_map(|(idx, entry)| entry.key.get(node_depth).map(|&c| (idx, c as usize)))
            .fold(Vec::new(), |mut ranges, (idx, child)| {
                match ranges.last_mut() {
                    Some(last) if last.0 == child && last.2 == idx => last.2 = idx + 1,
                    _ => ranges.push((child, idx, idx + 1)),
                }
                ranges
            })
    }

    /// Cold-cache root/cache rebuild over an already sorted nibble-keyed slice.
    fn memo_node_rebuild(
        entries: &[EntryRef<'_>],
        rp: &[u8],
        allow_path_compression: bool,
        cache_policy: CachePolicy,
    ) -> (H256, Vec<(Vec<u8>, H256)>) {
        debug_assert!(!entries.is_empty());
        let depth = rp.len();
        let first_key = entries[0].key;
        let last_key = entries[entries.len() - 1].key;
        let is_singleton = first_key == last_key;
        let common = if allow_path_compression {
            common_prefix_len(first_key, last_key, depth)
        } else {
            0
        };
        let node_depth = depth + common;
        let full_prefix = &first_key[0..node_depth];

        let maybe_value = entries
            .iter()
            .find(|entry| entry.key.len() == node_depth)
            .map(|entry| entry.value);

        let recurse = |&(child, start, end): &(usize, usize, usize)| {
            let child_rp = child_prefix(full_prefix, child);
            let (hash, cache_entries) =
                memo_node_rebuild(&entries[start..end], &child_rp, true, cache_policy);
            (child, hash, cache_entries)
        };

        let ranges = child_ranges(entries, node_depth);
        let results: Vec<(usize, H256, Vec<(Vec<u8>, H256)>)> =
            if entries.len() >= PARALLEL_REBUILD_THRESHOLD && ranges.len() > 1 {
                ranges.par_iter().map(&recurse).collect()
            } else {
                ranges.iter().map(&recurse).collect()
            };

        let mut children: [H256; CHILDREN] = [MERKLE_NULL_NODE; CHILDREN];
        let mut cache_entries = Vec::with_capacity(results.iter().map(|(_, _, c)| c.len()).sum());
        for (child, hash, mut child_cache_entries) in results {
            children[child] = hash;
            cache_entries.append(&mut child_cache_entries);
        }

        let has_children = children.iter().any(|h| *h != MERKLE_NULL_NODE);
        let node_hash = compute_node_merkle(has_children.then_some(&children), maybe_value);
        let hash = compute_path_merkle(&first_key[depth..node_depth], depth, node_hash);

        if cache_policy == CachePolicy::Full || !is_singleton {
            cache_entries.push((rp.to_vec(), hash));
        }
        (hash, cache_entries)
    }

    fn rebuild_snapshot_root_cache(
        snapshot: &BTreeMap<Vec<u8>, Box<[u8]>>,
        cache_policy: CachePolicy,
    ) -> (H256, Cache) {
        if snapshot.is_empty() {
            return (MERKLE_NULL_NODE, Cache::default());
        }
        let encoded: Vec<(Vec<u8>, &[u8])> = snapshot
            .iter()
            .map(|(key, value)| (bytes_to_nibbles(key), value.as_ref()))
            .collect();
        let entries: Vec<EntryRef<'_>> = encoded
            .iter()
            .map(|(key, value)| EntryRef {
                key: key.as_slice(),
                value,
            })
            .collect();
        let (root, cache_entries) = memo_node_rebuild(&entries, &[], false, cache_policy);
        (root, cache_entries.into_iter().collect())
    }

    /// Like [`rebuild_snapshot_root_cache`], but over entries already keyed by
    /// nibbles (as streamed from a checkpoint into LMDB), ascending-sorted.
    fn rebuild_snapshot_root_cache_from_nibble_entries(
        entries: &[(Vec<u8>, Vec<u8>)],
        cache_policy: CachePolicy,
    ) -> (H256, Cache) {
        if entries.is_empty() {
            return (MERKLE_NULL_NODE, Cache::default());
        }
        let refs: Vec<EntryRef<'_>> = entries
            .iter()
            .map(|(key, value)| EntryRef {
                key: key.as_slice(),
                value: value.as_slice(),
            })
            .collect();
        let (root, cache_entries) = memo_node_rebuild(&refs, &[], false, cache_policy);
        (root, cache_entries.into_iter().collect())
    }
}

#[cfg(feature = "snapshot-lmdb")]
pub(crate) use lmdb::SnapshotTrie;
