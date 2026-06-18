use crate::{
    trie::keccak,
    types::{Error, Result, Space, H256, MERKLE_NULL_NODE},
};
use serde::{Deserialize, Serialize};

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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeltaMptKeyPadding(pub [u8; KEY_PADDING_BYTES]);

impl Default for DeltaMptKeyPadding {
    fn default() -> Self {
        Self::genesis()
    }
}

impl DeltaMptKeyPadding {
    pub fn genesis() -> Self {
        Self::from_roots(MERKLE_NULL_NODE, MERKLE_NULL_NODE)
    }

    pub fn from_roots(snapshot_root: H256, intermediate_delta_root: H256) -> Self {
        let mut buffer = Vec::with_capacity(64);
        buffer.extend_from_slice(snapshot_root.as_bytes());
        buffer.extend_from_slice(intermediate_delta_root.as_bytes());
        Self(keccak(&buffer).0)
    }
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

    pub fn to_delta_mpt_key_bytes(&self, padding: &DeltaMptKeyPadding) -> Result<Vec<u8>> {
        let key = match &self.key {
            StorageKey::Account(address) => {
                if address.len() < ACCOUNT_BYTES {
                    return Ok(address.clone());
                }
                new_account_key(checked_address(address)?, padding)
            }
            StorageKey::StorageRoot(address) => {
                let mut key = new_account_key(checked_address(address)?, padding);
                key.extend_from_slice(STORAGE_PREFIX);
                key
            }
            StorageKey::Storage {
                address,
                storage_key,
            } => {
                let mut key = new_account_key(checked_address(address)?, padding);
                key.extend_from_slice(STORAGE_PREFIX);
                key.extend_from_slice(
                    &storage_key_padding(storage_key, padding).0[STORAGE_PREFIX.len()..],
                );
                key.extend_from_slice(storage_key);
                key
            }
            StorageKey::CodeRoot(address) => {
                let mut key = new_account_key(checked_address(address)?, padding);
                key.extend_from_slice(CODE_HASH_PREFIX);
                key
            }
            StorageKey::Code { address, code_hash } => {
                let mut key = new_account_key(checked_address(address)?, padding);
                key.extend_from_slice(CODE_HASH_PREFIX);
                key.extend_from_slice(code_hash);
                key
            }
            StorageKey::DepositList(address) => {
                let mut key = new_account_key(checked_address(address)?, padding);
                key.extend_from_slice(DEPOSIT_LIST_PREFIX);
                key
            }
            StorageKey::VoteList(address) => {
                let mut key = new_account_key(checked_address(address)?, padding);
                key.extend_from_slice(VOTE_LIST_PREFIX);
                key
            }
            StorageKey::Empty | StorageKey::AddressPrefix(_) => return Ok(Vec::new()),
        };
        Ok(add_space_to_delta_key(key, self.space))
    }

    pub fn from_delta_mpt_key(raw: &[u8]) -> Result<Self> {
        if raw.len() < ACCOUNT_KEYPART_BYTES {
            return Ok(Self::native(StorageKey::Account(raw.to_vec())));
        }
        let address = raw[ACCOUNT_PADDING_BYTES..ACCOUNT_KEYPART_BYTES].to_vec();
        if raw.len() == ACCOUNT_KEYPART_BYTES {
            return Ok(Self::native(StorageKey::Account(address)));
        }
        if raw.len() == ACCOUNT_KEYPART_BYTES + 1 && raw[ACCOUNT_KEYPART_BYTES] == EVM_SPACE_TYPE {
            return Ok(Self::ethereum(StorageKey::Account(address)));
        }

        let has_space = raw[ACCOUNT_KEYPART_BYTES] & 0x80 != 0;
        let (space, rest) = if has_space {
            if raw[ACCOUNT_KEYPART_BYTES] != EVM_SPACE_TYPE {
                return Err(Error::InvalidKey("unknown delta space marker"));
            }
            (Space::Ethereum, &raw[ACCOUNT_KEYPART_BYTES + 1..])
        } else {
            (Space::Native, &raw[ACCOUNT_KEYPART_BYTES..])
        };

        let key = if rest.starts_with(STORAGE_PREFIX) {
            if rest.len() == STORAGE_PREFIX.len() {
                StorageKey::StorageRoot(address)
            } else {
                if rest.len() < KEY_PADDING_BYTES {
                    return Err(Error::InvalidKey(
                        "storage delta key missing padded storage key",
                    ));
                }
                StorageKey::Storage {
                    address,
                    storage_key: rest[KEY_PADDING_BYTES..].to_vec(),
                }
            }
        } else if rest.starts_with(CODE_HASH_PREFIX) {
            let code_hash = &rest[CODE_HASH_PREFIX.len()..];
            if code_hash.is_empty() {
                StorageKey::CodeRoot(address)
            } else {
                StorageKey::Code {
                    address,
                    code_hash: code_hash.to_vec(),
                }
            }
        } else if rest.starts_with(DEPOSIT_LIST_PREFIX) {
            StorageKey::DepositList(address)
        } else if rest.starts_with(VOTE_LIST_PREFIX) {
            StorageKey::VoteList(address)
        } else {
            return Err(Error::InvalidKey("unknown delta key suffix"));
        };
        Ok(Self { key, space })
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

fn checked_address(address: &[u8]) -> Result<&[u8]> {
    if address.len() == ACCOUNT_BYTES {
        Ok(address)
    } else {
        Err(Error::InvalidKey("address must be 20 bytes"))
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

fn add_space_to_delta_key(mut key: Vec<u8>, space: Space) -> Vec<u8> {
    if space == Space::Ethereum {
        key.splice(
            ACCOUNT_KEYPART_BYTES..ACCOUNT_KEYPART_BYTES,
            [EVM_SPACE_TYPE],
        );
    }
    key
}

fn new_account_key(address: &[u8], padding: &DeltaMptKeyPadding) -> Vec<u8> {
    let mut padded = [0u8; ACCOUNT_KEYPART_BYTES];
    padded[..ACCOUNT_PADDING_BYTES].copy_from_slice(&padding.0[..ACCOUNT_PADDING_BYTES]);
    padded[ACCOUNT_PADDING_BYTES..].copy_from_slice(address);
    let hash = keccak(&padded);

    let mut key = Vec::with_capacity(ACCOUNT_KEYPART_BYTES);
    key.extend_from_slice(&hash.0[..ACCOUNT_PADDING_BYTES]);
    key.extend_from_slice(address);
    key
}

fn storage_key_padding(storage_key: &[u8], padding: &DeltaMptKeyPadding) -> DeltaMptKeyPadding {
    let mut padded = Vec::with_capacity(KEY_PADDING_BYTES + storage_key.len());
    padded.extend_from_slice(&padding.0);
    padded.extend_from_slice(storage_key);
    DeltaMptKeyPadding(keccak(&padded).0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn espace_marker_positions_differ() {
        let key = StorageKeyWithSpace::ethereum(StorageKey::Account(vec![7; 20]));
        assert_eq!(key.to_key_bytes().unwrap()[20], EVM_SPACE_TYPE);
        assert_eq!(
            key.to_delta_mpt_key_bytes(&DeltaMptKeyPadding::genesis())
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
            .to_delta_mpt_key_bytes(&DeltaMptKeyPadding::genesis())
            .unwrap();
        assert_eq!(StorageKeyWithSpace::from_delta_mpt_key(&raw).unwrap(), key);
    }
}
