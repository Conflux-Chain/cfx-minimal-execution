mod diag;
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
pub use persist::{load_state, save_state};
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

// `Error` is the crate-root aggregate: it folds the internal `keys::KeyError`
// and `persist::PersistError` sub-errors (and their underlying `io`/`bincode`
// sources) into a single flat type. `Display` and the `From` conversions below
// are wired so every case prints exactly as before — the sub-error layering is
// purely internal.
impl From<keys::KeyError> for Error {
    fn from(value: keys::KeyError) -> Self {
        match value {
            keys::KeyError::InvalidKey(msg) => Self::InvalidKey(msg),
        }
    }
}

impl From<persist::PersistError> for Error {
    fn from(value: persist::PersistError) -> Self {
        match value {
            persist::PersistError::Io(e) => Self::Io(e),
            persist::PersistError::Codec(e) => Self::Codec(e),
        }
    }
}

impl From<io::Error> for Error {
    fn from(value: io::Error) -> Self {
        persist::PersistError::from(value).into()
    }
}

impl From<Box<bincode::ErrorKind>> for Error {
    fn from(value: Box<bincode::ErrorKind>) -> Self {
        persist::PersistError::from(value).into()
    }
}

pub type Result<T> = std::result::Result<T, Error>;
