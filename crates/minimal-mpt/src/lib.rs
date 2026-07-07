mod diag;
mod keys;
mod merkle;
mod persist;
mod primitives;
mod state;
mod trie;

use std::{fmt, io};

pub(crate) use trie::nibbles;

pub use keys::{DeltaMptKeyPadding, StorageKey, StorageKeyWithSpace};
pub use persist::format::PersistedState;
pub use persist::{load_state, save_state};
pub use primitives::{Space, H256, MERKLE_NULL_NODE};
pub use state::{CommitRoot, State, StateTrait};
pub use trie::MptValue;

// The `incremental_trie` differential fuzz drives the two production layer
// wrappers — `SnapshotTrie` (two-level store, `SkipSingleton` policy) and
// `DeltaTrie` (BTree store, `Full` policy) — through the same op sequence and
// asserts both match the stateless oracle. The types are `pub` in crate-private
// modules, so these re-exports exist only under `fuzzing`; the normal public
// API is unchanged.
#[cfg(fuzzing)]
pub use state::layers::delta::DeltaTrie;
#[cfg(fuzzing)]
pub use state::layers::snapshot::SnapshotTrie;
#[cfg(fuzzing)]
pub use trie::trie_root;
// `MptKeyValue` (the `(key, value)` pair the prefix-scan APIs yield) is named only
// by the `layered_state_ops` fuzz harness. The `StateTrait` methods that return it
// stay callable off the public tuple type; downstream crates use their own alias.
#[cfg(fuzzing)]
pub use primitives::MptKeyValue;

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
