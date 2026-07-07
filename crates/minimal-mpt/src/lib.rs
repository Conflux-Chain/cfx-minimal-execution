mod keys;
mod merkle;
mod persist;
mod primitives;
mod state;
mod trie;

use std::{fmt, io};

pub(crate) use trie::nibbles;
pub(crate) use trie::root as incremental;
pub(crate) use trie::store::two_level as entries;

pub use keys::{DeltaMptKeyPadding, StorageKey, StorageKeyWithSpace};
pub use persist::format::PersistedState;
pub use primitives::{MptKeyValue, Space, H256, MERKLE_NULL_NODE};
pub use state::{CommitRoot, State, StateTrait};
pub use trie::MptValue;

#[cfg(fuzzing)]
pub use incremental::IncrementalTrie;
#[cfg(fuzzing)]
pub use trie::trie_root;

#[derive(Debug)]
pub enum Error {
    InvalidKey(&'static str),
    Io(io::Error),
    Codec(Box<bincode::ErrorKind>),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidKey(msg) => write!(f, "invalid key: {msg}"),
            Self::Io(e) => write!(f, "io error: {e}"),
            Self::Codec(e) => write!(f, "codec error: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<Box<bincode::ErrorKind>> for Error {
    fn from(value: Box<bincode::ErrorKind>) -> Self {
        Self::Codec(value)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
