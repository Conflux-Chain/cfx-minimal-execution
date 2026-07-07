use crate::primitives::Space;
use crate::Result;
use serde::{Deserialize, Serialize};
use std::fmt;

mod account_cache;
mod canonical;
mod delta;

pub(crate) use account_cache::AccountKeyCache;
pub use delta::DeltaMptKeyPadding;

/// Key-codec sub-error: a storage key whose bytes could not be encoded or
/// decoded (wrong length, unknown suffix/marker). Folded into the crate-root
/// [`crate::Error`] aggregate via `From`; `Display` matches the aggregate's
/// legacy output for this case exactly.
#[derive(Debug)]
pub(crate) enum KeyError {
    InvalidKey(&'static str),
}

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidKey(msg) => write!(f, "invalid key: {msg}"),
        }
    }
}

impl std::error::Error for KeyError {}

const ACCOUNT_BYTES: usize = 20;
const ACCOUNT_KEYPART_BYTES: usize = 32;
const ACCOUNT_PADDING_BYTES: usize = 12;
const KEY_PADDING_BYTES: usize = 32;
const EVM_SPACE_TYPE: u8 = 0x81;
const STORAGE_PREFIX: &[u8] = b"data";
const CODE_HASH_PREFIX: &[u8] = b"code";
const DEPOSIT_LIST_PREFIX: &[u8] = b"deposit";
const VOTE_LIST_PREFIX: &[u8] = b"vote";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
// LLM: change to &[u8] and StorageKey will have a lifetime StorageKey<'a>
pub enum StorageKey {
    Account(Vec<u8>),
    StorageRoot(Vec<u8>),
    Storage {
        address: Vec<u8>,
        storage_key: Vec<u8>,
    },
    CodeRoot(Vec<u8>),
    Code {
        address: Vec<u8>,
        code_hash: Vec<u8>,
    },
    DepositList(Vec<u8>),
    VoteList(Vec<u8>),
    Empty,
    AddressPrefix(Vec<u8>),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageKeyWithSpace {
    pub key: StorageKey,
    pub space: Space,
}

impl StorageKeyWithSpace {
    pub fn native(key: StorageKey) -> Self {
        Self {
            key,
            space: Space::Native,
        }
    }

    pub fn ethereum(key: StorageKey) -> Self {
        Self {
            key,
            space: Space::Ethereum,
        }
    }
}

fn checked_address(address: &[u8]) -> Result<&[u8]> {
    if address.len() == ACCOUNT_BYTES {
        Ok(address)
    } else {
        Err(KeyError::InvalidKey("address must be 20 bytes").into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn espace_marker_positions_differ() {
        let key = StorageKeyWithSpace::ethereum(StorageKey::Account(vec![7; 20]));
        assert_eq!(key.to_key_bytes().unwrap()[20], EVM_SPACE_TYPE);
        assert_eq!(
            key.to_delta_mpt_key_bytes(&DeltaMptKeyPadding::genesis(), None)
                .unwrap()[32],
            EVM_SPACE_TYPE
        );
    }

    #[test]
    fn delta_key_roundtrip() {
        let key = StorageKeyWithSpace::ethereum(StorageKey::Storage {
            address: vec![1; 20],
            storage_key: vec![2; 32],
        });
        let raw = key
            .to_delta_mpt_key_bytes(&DeltaMptKeyPadding::genesis(), None)
            .unwrap();
        assert_eq!(StorageKeyWithSpace::from_delta_mpt_key(&raw).unwrap(), key);
    }
}
