mod codec;
mod key_codec;
mod merkle;
mod snapshot;
mod state;
mod store;
mod trie;
mod types;

pub(crate) use trie::nibbles;
pub(crate) use trie::root as incremental;
pub(crate) use trie::store::two_level as entries;

pub use key_codec::{DeltaMptKeyPadding, StorageKey, StorageKeyWithSpace};
pub use state::{State, StateTrait};
pub use store::PersistedState;
pub use trie::MptValue;
pub use types::{CommitRoot, Error, MptKeyValue, Result, Space, H256, MERKLE_NULL_NODE};

#[cfg(fuzzing)]
pub use incremental::IncrementalTrie;
#[cfg(fuzzing)]
pub use trie::trie_root;
