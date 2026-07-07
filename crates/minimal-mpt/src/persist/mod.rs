pub(crate) mod format;
pub(crate) mod stream;
pub(crate) mod wire;

pub use stream::{load_state, save_state};

use std::{fmt, io};

/// Persistence-layer sub-error: a streaming I/O failure or a bincode wire-codec
/// failure on the checkpoint save/load paths. Folded into the crate-root
/// [`crate::Error`] aggregate via `From`; `Display` matches the aggregate's
/// legacy output for these cases exactly.
#[derive(Debug)]
pub(crate) enum PersistError {
    Io(io::Error),
    Codec(Box<bincode::ErrorKind>),
}

impl fmt::Display for PersistError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io error: {e}"),
            Self::Codec(e) => write!(f, "codec error: {e}"),
        }
    }
}

impl std::error::Error for PersistError {}

impl From<io::Error> for PersistError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<Box<bincode::ErrorKind>> for PersistError {
    fn from(value: Box<bincode::ErrorKind>) -> Self {
        Self::Codec(value)
    }
}
