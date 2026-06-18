use serde::{Deserialize, Serialize};
use std::{fmt, io};

#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct H256(pub [u8; 32]);

impl H256 {
    pub const fn zero() -> Self {
        Self([0u8; 32])
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for H256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x")?;
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

pub const MERKLE_NULL_NODE: H256 = H256([
    0xc5, 0xd2, 0x46, 0x01, 0x86, 0xf7, 0x23, 0x3c, 0x92, 0x7e, 0x7d, 0xb2, 0xdc, 0xc7, 0x03, 0xc0,
    0xe5, 0x00, 0xb6, 0x53, 0xca, 0x82, 0x27, 0x3b, 0x7b, 0xfa, 0xd8, 0x04, 0x5d, 0x85, 0xa4, 0x70,
]);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Space {
    Native,
    Ethereum,
}

pub type MptKeyValue = (Vec<u8>, Box<[u8]>);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitRoot {
    pub snapshot_root: H256,
    pub intermediate_delta_root: H256,
    pub delta_root: H256,
    pub state_root_hash: H256,
    pub delta_mpt_key_padding: [u8; 32],
}

impl CommitRoot {
    pub fn new(
        snapshot_root: H256,
        intermediate_delta_root: H256,
        delta_root: H256,
        delta_mpt_key_padding: [u8; 32],
    ) -> Self {
        let mut buf = Vec::with_capacity(96);
        buf.extend_from_slice(snapshot_root.as_bytes());
        buf.extend_from_slice(intermediate_delta_root.as_bytes());
        buf.extend_from_slice(delta_root.as_bytes());
        let state_root_hash = crate::trie::keccak(&buf);
        Self {
            snapshot_root,
            intermediate_delta_root,
            delta_root,
            state_root_hash,
            delta_mpt_key_padding,
        }
    }
}

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
