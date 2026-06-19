pub use crate::packet::{
    BlockInput as RawBlockData, PacketInput as RawExecutionData,
    PosLookupEntry as RawPosLookupEntry, SenderBaseNonce as RawSenderBaseNonce,
};

use anyhow::Result;

pub fn encode_raw_data(raw: &RawExecutionData) -> Result<Vec<u8>> {
    crate::packet::encode_packet(raw)
}
