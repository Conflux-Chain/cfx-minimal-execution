pub(crate) mod nibbles;
pub(crate) mod root;
pub(crate) mod store;
mod value;

pub use value::{MptValue, TrieValue};

pub use crate::merkle::oracle::trie_root;
#[cfg(feature = "verify-incremental")]
pub(crate) use crate::merkle::oracle::{bytes_to_nibbles, root_for_entries};
pub(crate) use crate::merkle::rules::{compute_node_merkle, compute_path_merkle, keccak, CHILDREN};
