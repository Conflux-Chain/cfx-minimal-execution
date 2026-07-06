use anyhow::Result;
use std::path::PathBuf;

fn main() -> Result<()> {
    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .expect("usage: ckpt-roundtrip <checkpoint.bin>");
    let (full_ok, stream_ok) = cfx_replay_checkpoint::verify_roundtrip(&path)?;
    if !full_ok || !stream_ok {
        std::process::exit(1);
    }
    Ok(())
}
