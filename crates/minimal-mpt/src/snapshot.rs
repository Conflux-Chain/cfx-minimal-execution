use crate::{incremental::CachePolicy, trie::MptValue, types::H256};
use std::collections::BTreeMap;

#[cfg(not(feature = "snapshot-lmdb"))]
#[derive(Clone, Debug, Default)]
pub(crate) struct SnapshotTrie {
    inner: crate::incremental::IncrementalTrie,
}

#[cfg(not(feature = "snapshot-lmdb"))]
impl SnapshotTrie {
    pub(crate) fn from_snapshot(snapshot: &BTreeMap<Vec<u8>, Box<[u8]>>) -> Self {
        Self {
            inner: crate::incremental::IncrementalTrie::from_snapshot(snapshot),
        }
    }

    pub(crate) fn insert(&mut self, key: &[u8], value: MptValue) {
        self.inner.insert(key, value);
    }

    pub(crate) fn remove(&mut self, key: &[u8]) {
        self.inner.remove(key);
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

    pub(crate) fn len(&self) -> usize {
        self.inner.len()
    }
}

#[cfg(feature = "snapshot-lmdb")]
mod lmdb {
    use super::*;
    use crate::{
        incremental::{common_prefix_len, nibbles_to_bytes, rebuild_snapshot_root_cache},
        trie::{bytes_to_nibbles, compute_node_merkle, compute_path_merkle, CHILDREN},
        types::MERKLE_NULL_NODE,
    };
    use heed::{types::Bytes, Database, Env, EnvOpenOptions};
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
}

#[cfg(feature = "snapshot-lmdb")]
pub(crate) use lmdb::SnapshotTrie;
