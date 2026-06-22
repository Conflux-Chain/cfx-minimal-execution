//! Prefix reads and deletes across the three-layer state.
//!
//! A prefix query unions matches from all three layers with delta-over-
//! intermediate-over-snapshot precedence (first layer to yield a canonical key
//! wins). `AddressPrefix` queries additionally re-filter on the canonical key,
//! since the delta-mpt prefix is coarser than the address prefix.

use super::{State, StateTrait};
use crate::{
    key_codec::{StorageKey, StorageKeyWithSpace},
    trie::MptValue,
    types::{MptKeyValue, Result},
};
use std::collections::{BTreeMap, HashSet};

impl State {
    pub(super) fn read_prefix(&self, prefix: StorageKeyWithSpace) -> Result<Vec<MptKeyValue>> {
        let canonical_prefix = prefix.to_key_bytes()?;
        let delta_prefix = prefix.to_delta_mpt_key_bytes(&self.delta_padding, None)?;
        let intermediate_prefix =
            prefix.to_delta_mpt_key_bytes(&self.intermediate_padding, None)?;
        let address_prefix = address_prefix_filter(&prefix);

        let mut result = Vec::new();
        let mut seen = HashSet::new();

        for (raw_key, value) in scan_prefix(&self.delta, &delta_prefix) {
            let canonical = StorageKeyWithSpace::from_delta_mpt_key(raw_key)?.to_key_bytes()?;
            if address_prefix.is_some_and(|prefix| !canonical.starts_with(prefix)) {
                continue;
            }
            seen.insert(canonical.clone());
            if let Some(value) = value.visible_value() {
                result.push((canonical, Box::from(value)));
            }
        }

        for (raw_key, value) in scan_prefix(&self.intermediate, &intermediate_prefix) {
            let canonical = StorageKeyWithSpace::from_delta_mpt_key(raw_key)?.to_key_bytes()?;
            if address_prefix.is_some_and(|prefix| !canonical.starts_with(prefix)) {
                continue;
            }
            if seen.insert(canonical.clone()) {
                if let Some(value) = value.visible_value() {
                    result.push((canonical, Box::from(value)));
                }
            }
        }

        for (key, value) in self.snapshot.snapshot_scan_prefix(&canonical_prefix) {
            if seen.insert(key.clone()) {
                result.push((key, value));
            }
        }

        Ok(result)
    }

    pub(super) fn delete_prefix(
        &mut self,
        prefix: StorageKeyWithSpace,
    ) -> Result<Vec<MptKeyValue>> {
        let canonical_prefix = prefix.to_key_bytes()?;
        let delta_prefix = prefix.to_delta_mpt_key_bytes(&self.delta_padding, None)?;
        let intermediate_prefix =
            prefix.to_delta_mpt_key_bytes(&self.intermediate_padding, None)?;
        let address_prefix = address_prefix_filter(&prefix).map(Vec::from);

        let delta_keys: Vec<_> = scan_prefix(&self.delta, &delta_prefix)
            .map(|(raw_key, _)| raw_key.clone())
            .collect();
        let mut delta_kvs = Vec::with_capacity(delta_keys.len());
        for raw_key in delta_keys {
            if let Some(value) = self.delta.remove(&raw_key) {
                self.delta_inc.remove(&raw_key);
                delta_kvs.push((raw_key, value));
            }
        }

        let intermediate_kvs: Vec<_> = scan_prefix(&self.intermediate, &intermediate_prefix)
            .map(|(raw_key, value)| (raw_key.clone(), value.clone()))
            .collect();
        let snapshot_kvs: Vec<_> = self.snapshot.snapshot_scan_prefix(&canonical_prefix);

        let mut result = Vec::new();
        let mut seen = HashSet::new();

        for (raw_key, value) in delta_kvs {
            let canonical = StorageKeyWithSpace::from_delta_mpt_key(&raw_key)?.to_key_bytes()?;
            if address_prefix
                .as_deref()
                .is_some_and(|prefix| !canonical.starts_with(prefix))
            {
                continue;
            }
            seen.insert(canonical.clone());
            if let Some(value) = value.visible_value() {
                result.push((canonical, Box::from(value)));
            }
        }

        for (raw_key, value) in intermediate_kvs {
            let storage_key = StorageKeyWithSpace::from_delta_mpt_key(&raw_key)?;
            let canonical = storage_key.to_key_bytes()?;
            if address_prefix
                .as_deref()
                .is_some_and(|prefix| !canonical.starts_with(prefix))
            {
                continue;
            }
            if value.visible_value().is_some() {
                self.set(storage_key, Box::new([]))?;
            }
            if seen.insert(canonical.clone()) {
                if let Some(value) = value.visible_value() {
                    result.push((canonical, Box::from(value)));
                }
            }
        }

        for (canonical, value) in snapshot_kvs {
            let storage_key = StorageKeyWithSpace::from_key_bytes(&canonical)?;
            self.set(storage_key, Box::new([]))?;
            if seen.insert(canonical.clone()) {
                result.push((canonical, value));
            }
        }

        Ok(result)
    }
}

fn scan_prefix<'a>(
    map: &'a BTreeMap<Vec<u8>, MptValue>,
    prefix: &'a [u8],
) -> impl Iterator<Item = (&'a Vec<u8>, &'a MptValue)> {
    map.range(prefix.to_vec()..)
        .take_while(move |(key, _)| key.starts_with(prefix))
}

fn address_prefix_filter(prefix: &StorageKeyWithSpace) -> Option<&[u8]> {
    match &prefix.key {
        StorageKey::AddressPrefix(prefix) => Some(prefix),
        _ => None,
    }
}
