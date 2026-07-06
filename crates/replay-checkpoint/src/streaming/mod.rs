//! Production streaming paths — save and load without materializing the full
//! snapshot `BTreeMap` in memory.

mod load;
mod save;

pub use load::CheckpointData;
pub use save::CheckpointRef;
