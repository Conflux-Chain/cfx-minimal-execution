//! Prefix reads and deletes across the three-layer state.
//!
//! A prefix query unions matches from all three layers with delta-over-
//! intermediate-over-snapshot precedence (first layer to yield a canonical key
//! wins). `AddressPrefix` queries additionally re-filter on the canonical key,
//! since the delta-mpt prefix is coarser than the address prefix.

use super::{State, StateTrait};
use crate::diag::profile;
use crate::{
    keys::{StorageKey, StorageKeyWithSpace},
    primitives::MptKeyValue,
    trie::MptValue,
    Result,
};
use std::collections::HashSet;

impl State {
    pub(super) fn read_prefix(&self, prefix: StorageKeyWithSpace) -> Result<Vec<MptKeyValue>> {
        let mut t = profile::op(profile::Op::Scan);

        let canonical_prefix = prefix.to_key_bytes()?;
        let delta_prefix = prefix.to_delta_mpt_key_bytes(&self.delta_padding, None)?;
        let intermediate_prefix =
            prefix.to_delta_mpt_key_bytes(&self.intermediate_padding, None)?;
        let address_prefix = address_prefix_filter(&prefix);

        let mut result = Vec::new();
        let mut seen = HashSet::new();

        // Delta then intermediate: same (delta-mpt-padded, `BTreeStore`-backed)
        // representation, so one parameterized pass each with delta-over-
        // intermediate precedence (first layer to claim a canonical key wins).
        collect_layer_prefix(
            self.delta.scan_prefix(&delta_prefix),
            address_prefix,
            &mut seen,
            &mut result,
        )?;
        collect_layer_prefix(
            self.intermediate.scan_prefix(&intermediate_prefix),
            address_prefix,
            &mut seen,
            &mut result,
        )?;

        for (key, value) in self.snapshot.snapshot_scan_prefix(&canonical_prefix) {
            if seen.insert(key.clone()) {
                result.push((key, value));
            }
        }

        t.entries = result.len() as u64;
        Ok(result)
    }

    pub(super) fn delete_prefix(
        &mut self,
        prefix: StorageKeyWithSpace,
    ) -> Result<Vec<MptKeyValue>> {
        let mut t = profile::op(profile::Op::Scan);

        let canonical_prefix = prefix.to_key_bytes()?;
        let delta_prefix = prefix.to_delta_mpt_key_bytes(&self.delta_padding, None)?;
        let intermediate_prefix =
            prefix.to_delta_mpt_key_bytes(&self.intermediate_padding, None)?;
        let address_prefix = address_prefix_filter(&prefix).map(Vec::from);

        // Collect the matching delta entries (key + value) first, then remove them
        // — every scanned key is present, so this is the old "remove, push if it
        // was there" with the presence check already discharged by the scan.
        let delta_kvs: Vec<(Vec<u8>, MptValue)> = self
            .delta
            .scan_prefix(&delta_prefix)
            .map(|(raw_key, value)| (raw_key.to_vec(), value.clone()))
            .collect();
        for (raw_key, _) in &delta_kvs {
            self.delta.remove(raw_key);
        }

        let intermediate_kvs: Vec<(Vec<u8>, MptValue)> = self
            .intermediate
            .scan_prefix(&intermediate_prefix)
            .map(|(raw_key, value)| (raw_key.to_vec(), value.clone()))
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

        t.entries = result.len() as u64;
        Ok(result)
    }
}

/// One delta-mpt layer's contribution to a prefix read: convert each scanned raw
/// key back to canonical, apply the optional address-prefix re-filter, and record
/// the first layer to claim each canonical key (a tombstone still claims the key,
/// hiding lower layers, but contributes no value). Parameterized over the layer's
/// scan iterator so delta and intermediate — now the same representation — share
/// this one path.
fn collect_layer_prefix<'a>(
    entries: impl Iterator<Item = (&'a [u8], &'a MptValue)>,
    address_prefix: Option<&[u8]>,
    seen: &mut HashSet<Vec<u8>>,
    result: &mut Vec<MptKeyValue>,
) -> Result<()> {
    for (raw_key, value) in entries {
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
    Ok(())
}

fn address_prefix_filter(prefix: &StorageKeyWithSpace) -> Option<&[u8]> {
    match &prefix.key {
        StorageKey::AddressPrefix(prefix) => Some(prefix),
        _ => None,
    }
}
