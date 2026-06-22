// The `.cfxpack` wire format (packet/codec/decode/verify) lives in the shared
// `cfxpack` crate, used directly at each call site.
pub mod bench;
pub mod cli;
pub mod extract;
pub mod validate;

pub use cfxpack::verify::VerifyReport;
pub use extract::{ExtractConfig, ExtractReport};
pub use validate::{ReplayReport, ReplayValidationReport};
