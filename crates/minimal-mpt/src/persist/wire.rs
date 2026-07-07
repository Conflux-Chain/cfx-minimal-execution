//! Low-level bincode wire-format helpers for streaming checkpoint I/O.
//!
//! bincode 1.x encodes `Vec<u8>` / `Box<[u8]>` as a fixed LE `u64` length
//! prefix followed by raw bytes. These helpers read that encoding one field at
//! a time, so the checkpoint loader can stream entries without buffering the
//! whole file.

use std::io::{self, Read, Write};

const MAX_LEN_PREFIXED_BYTES: u64 = 128 * 1024 * 1024;

/// Read a little-endian `u64` (bincode 1.x fixint length encoding).
pub(crate) fn read_u64_le(r: &mut impl Read) -> io::Result<u64> {
    let mut buf = [0u8; 8];
    r.read_exact(&mut buf)?;
    Ok(u64::from_le_bytes(buf))
}

/// Write a little-endian `u64` (bincode 1.x fixint length encoding).
pub(crate) fn write_u64_le(w: &mut impl Write, v: u64) -> io::Result<()> {
    w.write_all(&v.to_le_bytes())
}

/// Read a bincode length-prefixed byte string (`u64 len` + raw bytes) — the
/// on-disk form of `Vec<u8>` / `Box<[u8]>` under bincode 1.x default options.
pub(crate) fn read_len_prefixed(r: &mut impl Read) -> io::Result<Vec<u8>> {
    let len = read_u64_le(r)?;
    if len > MAX_LEN_PREFIXED_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("length-prefixed field too large: {len} bytes"),
        ));
    }
    let len = usize::try_from(len).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "length-prefixed field does not fit in usize",
        )
    })?;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

/// Write a bincode length-prefixed byte string (`u64 len` + raw bytes).
pub(crate) fn write_len_prefixed(w: &mut impl Write, data: &[u8]) -> io::Result<()> {
    write_u64_le(w, data.len() as u64)?;
    w.write_all(data)
}
