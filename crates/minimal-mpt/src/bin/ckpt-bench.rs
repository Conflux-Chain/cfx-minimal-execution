// Streaming checkpoint save/load micro-benchmark (crates/minimal-mpt only, no
// oracle). Loads a real executor checkpoint via the PRODUCTION streaming path —
// a BufReader straight over the file, exactly like `Checkpoint::load_streaming`
// (NO `fs::read` of the whole file into RAM) — so the measured peak RSS is the
// real resident state, not a benchmark artifact. Then re-serialises via
// `write_streaming`. Run with MMPT_TIMING=1 for the per-phase breakdown.
//
// Usage: ckpt-bench <ckpt.bin> [tmp_save_path]
use cfx_minimal_mpt::State;
use std::error::Error;
use std::io::{self, BufReader, BufWriter, Read};
use std::time::Instant;

/// Counts bytes pulled through it — so we can report the streamed mmpt size
/// without materialising the file in RAM.
struct CountReader<R> {
    inner: R,
    n: u64,
}
impl<R: Read> Read for CountReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let k = self.inner.read(buf)?;
        self.n += k as u64;
        Ok(k)
    }
}

fn rss_gib() -> f64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines().find_map(|l| {
                l.strip_prefix("VmRSS:")
                    .and_then(|v| v.split_whitespace().next())
                    .and_then(|kb| kb.parse::<f64>().ok())
            })
        })
        .map(|kb| kb / 1048576.0)
        .unwrap_or(0.0)
}

fn mbps(bytes: u64, secs: f64) -> f64 {
    bytes as f64 / 1e6 / secs.max(1e-9)
}

fn main() -> Result<(), Box<dyn Error>> {
    let ckpt = std::env::args()
        .nth(1)
        .expect("usage: ckpt-bench <ckpt.bin> [tmp]");
    let tmp = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "/tmp/mmpt_stream_bench.bin".to_string());
    let file_len = std::fs::metadata(&ckpt)?.len();
    eprintln!(
        "ckpt file: {} bytes  start RSS={:.1}GiB",
        file_len,
        rss_gib()
    );

    // ---- STREAMING LOAD, production-style: BufReader straight over the file. ----
    // NO fs::read of the whole file — the only large resident object is the State.
    let mut r = CountReader {
        inner: BufReader::with_capacity(1 << 22, std::fs::File::open(&ckpt)?),
        n: 0,
    };
    // executor prefixes the mmpt stream with bincode(u32 version)+bincode(u64 height)
    let (_ver, _height): (u32, u64) = bincode::deserialize_from(&mut r)?;
    let prefix = r.n;
    let t = Instant::now();
    let state = State::from_reader(&mut r)?; // [mmpt] recover.* + rebuild.* events under MMPT_TIMING
    let load_secs = t.elapsed().as_secs_f64();
    let mmpt_bytes = r.n - prefix;
    drop(r); // release the BufReader
    eprintln!(
        "LOAD (streaming from_reader, BufReader over file): {:.2}s  mmpt_bytes={}  {:.0} MB/s  RSS={:.1}GiB",
        load_secs,
        mmpt_bytes,
        mbps(mmpt_bytes, load_secs),
        rss_gib()
    );

    // ---- STREAMING SAVE (write_streaming) ----
    let t = Instant::now();
    {
        let mut w = BufWriter::with_capacity(1 << 22, std::fs::File::create(&tmp)?);
        state.write_streaming(&mut w)?;
        use std::io::Write;
        w.flush()?;
    }
    let save_secs = t.elapsed().as_secs_f64();
    let save_bytes = std::fs::metadata(&tmp)?.len();
    eprintln!(
        "SAVE (streaming write_streaming): {:.2}s  bytes={}  {:.0} MB/s  RSS={:.1}GiB",
        save_secs,
        save_bytes,
        mbps(save_bytes, save_secs),
        rss_gib()
    );

    // NO reload — holding a second full State while the first is alive doubles
    // resident memory and OOMs the box. `state` stays alive through the save.
    std::hint::black_box(&state);
    let _ = std::fs::remove_file(&tmp);
    Ok(())
}
