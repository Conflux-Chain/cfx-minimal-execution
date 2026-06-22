//! `cfxpack` — the `.cfxpack` replay-data wire format.
//!
//! This is the format layer shared by the extractor (which encodes node data
//! into packets) and the executor (which decodes them for replay). It is the
//! single source of truth for the byte layout, so the two sides cannot drift.
//!
//! It carries no node-DB or EVM dependencies — only the codec and the
//! `primitives` transaction types the packet embeds. The two consumers resolve
//! `primitives`/`cfx-types` from different sources (the extractor pins a git
//! rev; the executor's workspace `[patch]`es them to its own crates), but
//! because each consumer builds independently, this crate simply compiles
//! against whichever the active build provides.

pub mod codec;
pub mod container;
pub mod decode;
pub mod packet;
pub mod verify;
