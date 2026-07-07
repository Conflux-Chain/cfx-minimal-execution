use super::*;
use crate::primitives::Space;
use crate::{Error, Result};

impl StorageKeyWithSpace {
    pub fn to_key_bytes(&self) -> Result<Vec<u8>> {
        let key = match &self.key {
            StorageKey::Account(address) => {
                if address.len() < ACCOUNT_BYTES {
                    return Ok(address.clone());
                }
                checked_address(address)?.to_vec()
            }
            StorageKey::StorageRoot(address) => {
                append_prefix(checked_address(address)?, STORAGE_PREFIX)
            }
            StorageKey::Storage {
                address,
                storage_key,
            } => {
                let mut key = append_prefix(checked_address(address)?, STORAGE_PREFIX);
                key.extend_from_slice(storage_key);
                key
            }
            StorageKey::CodeRoot(address) => {
                append_prefix(checked_address(address)?, CODE_HASH_PREFIX)
            }
            StorageKey::Code { address, code_hash } => {
                let mut key = append_prefix(checked_address(address)?, CODE_HASH_PREFIX);
                key.extend_from_slice(code_hash);
                key
            }
            StorageKey::DepositList(address) => {
                append_prefix(checked_address(address)?, DEPOSIT_LIST_PREFIX)
            }
            StorageKey::VoteList(address) => {
                append_prefix(checked_address(address)?, VOTE_LIST_PREFIX)
            }
            StorageKey::Empty => return Ok(Vec::new()),
            StorageKey::AddressPrefix(prefix) => return Ok(prefix.clone()),
        };
        Ok(add_space_to_snapshot_key(key, self.space))
    }

    pub fn from_key_bytes(raw: &[u8]) -> Result<Self> {
        if raw.is_empty() {
            return Ok(Self::native(StorageKey::Empty));
        }
        if raw.len() < ACCOUNT_BYTES {
            return Ok(Self::native(StorageKey::AddressPrefix(raw.to_vec())));
        }

        let (space, address, rest) =
            if raw.len() > ACCOUNT_BYTES && raw[ACCOUNT_BYTES] == EVM_SPACE_TYPE {
                (
                    Space::Ethereum,
                    raw[..ACCOUNT_BYTES].to_vec(),
                    &raw[ACCOUNT_BYTES + 1..],
                )
            } else {
                (
                    Space::Native,
                    raw[..ACCOUNT_BYTES].to_vec(),
                    &raw[ACCOUNT_BYTES..],
                )
            };

        let key = if rest.is_empty() {
            StorageKey::Account(address)
        } else if rest.starts_with(STORAGE_PREFIX) {
            let suffix = &rest[STORAGE_PREFIX.len()..];
            if suffix.is_empty() {
                StorageKey::StorageRoot(address)
            } else {
                StorageKey::Storage {
                    address,
                    storage_key: suffix.to_vec(),
                }
            }
        } else if rest.starts_with(CODE_HASH_PREFIX) {
            let suffix = &rest[CODE_HASH_PREFIX.len()..];
            if suffix.is_empty() {
                StorageKey::CodeRoot(address)
            } else {
                StorageKey::Code {
                    address,
                    code_hash: suffix.to_vec(),
                }
            }
        } else if rest == DEPOSIT_LIST_PREFIX {
            StorageKey::DepositList(address)
        } else if rest == VOTE_LIST_PREFIX {
            StorageKey::VoteList(address)
        } else {
            return Err(Error::InvalidKey("unknown snapshot key suffix"));
        };
        Ok(Self { key, space })
    }
}

fn append_prefix(address: &[u8], prefix: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(address.len() + prefix.len());
    key.extend_from_slice(address);
    key.extend_from_slice(prefix);
    key
}

fn add_space_to_snapshot_key(mut key: Vec<u8>, space: Space) -> Vec<u8> {
    if space == Space::Ethereum {
        key.splice(ACCOUNT_BYTES..ACCOUNT_BYTES, [EVM_SPACE_TYPE]);
    }
    key
}
