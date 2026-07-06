//! Checkpoint / resume for the minimal-mpt replay backend.
//!
//! ## Public API
//!
//! - [`CheckpointData`] — owned checkpoint state, returned by
//!   [`CheckpointData::load_streaming`] and consumed by the executor on resume.
//! - [`CheckpointRef`] — borrowed view of executor state, used by
//!   [`CheckpointRef::save_streaming`] for zero-copy production saves.
//! - [`verify_roundtrip`] / [`verify_streaming_matches`] — verification that
//!   the streaming path is byte-identical / structurally identical to the
//!   legacy full-materialization path.
//!
//! The legacy non-streaming [`Checkpoint`](legacy::Checkpoint) type is
//! `pub(crate)` — it defines the on-disk bincode format and serves as the
//! ground-truth reference for tests, but is not part of the production API.

pub(crate) mod legacy;
mod streaming;
mod types;
mod verify;
#[cfg(test)]
mod tests;

pub use streaming::{CheckpointData, CheckpointRef};
pub use types::{EpochCommitment, ExecutedEpoch};
pub use verify::{verify_roundtrip, verify_streaming_matches};

pub(crate) const CHECKPOINT_VERSION: u32 = 2;
