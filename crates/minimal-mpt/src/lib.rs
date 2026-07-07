mod codec;
mod entries;
mod incremental;
mod key_codec;
mod nibbles;
mod snapshot;
mod state;
mod store;
mod trie;
mod types;

pub use key_codec::{DeltaMptKeyPadding, StorageKey, StorageKeyWithSpace};
pub use state::{State, StateManager, StateTrait};
pub use store::{FileStore, MemoryStore, PersistedState, StateStore};
pub use trie::MptValue;
pub use types::{CommitRoot, Error, MptKeyValue, Result, Space, H256, MERKLE_NULL_NODE};

#[cfg(fuzzing)]
pub use incremental::IncrementalTrie;
#[cfg(fuzzing)]
pub use trie::trie_root;
