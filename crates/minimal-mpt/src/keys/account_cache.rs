use super::{DeltaMptKeyPadding, ACCOUNT_KEYPART_BYTES, ACCOUNT_PADDING_BYTES};
use crate::trie::keccak;
use rustc_hash::FxHashMap;

/// Per-period cache of [`new_account_key`] results, keyed by account address.
/// The `new_account_key` keccak is the hottest derivation in replay (every state
/// read/write that touches an account/its storage/code re-hashes the same address
/// with the period's fixed padding). Stamped with the padding it was built for: a
/// request with a different padding clears it, so the cache auto-invalidates when
/// `padding` rotates at a period boundary — no reliance on an external `clear`.
/// One instance per active padding (delta, intermediate).
#[derive(Debug, Default)]
pub struct AccountKeyCache {
    built_for: DeltaMptKeyPadding,
    map: FxHashMap<Vec<u8>, Vec<u8>>,
}

fn account_key_uncached(address: &[u8], padding: &DeltaMptKeyPadding) -> Vec<u8> {
    let mut padded = [0u8; ACCOUNT_KEYPART_BYTES];
    padded[..ACCOUNT_PADDING_BYTES].copy_from_slice(&padding.0[..ACCOUNT_PADDING_BYTES]);
    padded[ACCOUNT_PADDING_BYTES..].copy_from_slice(address);
    let hash = keccak(&padded);

    let mut key = Vec::with_capacity(ACCOUNT_KEYPART_BYTES);
    key.extend_from_slice(&hash.0[..ACCOUNT_PADDING_BYTES]);
    key.extend_from_slice(address);
    key
}

pub(super) fn new_account_key(
    address: &[u8],
    padding: &DeltaMptKeyPadding,
    cache: Option<&mut AccountKeyCache>,
) -> Vec<u8> {
    let Some(cache) = cache else {
        return account_key_uncached(address, padding);
    };
    // Padding stamp: if it changed (period boundary), the old entries are stale.
    if cache.built_for != *padding {
        cache.map.clear();
        cache.built_for = padding.clone();
    }
    if let Some(hit) = cache.map.get(address) {
        return hit.clone();
    }
    let key = account_key_uncached(address, padding);
    cache.map.insert(address.to_vec(), key.clone());
    key
}
