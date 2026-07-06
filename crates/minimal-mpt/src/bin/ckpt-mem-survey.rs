// Memory-optimization survey (crates/minimal-mpt only). Streams a real executor
// checkpoint in via the PRODUCTION path (BufReader straight over the file — no
// fs::read of the whole file into RAM), then does ONE O(1)-space pass over the
// snapshot `entries` (and cache) to measure:
//   - key / value / cache-key total bytes (the raw heap data)
//   - (b) shared first-40-nibble (20-byte) routing-prefix dedup potential
//   - (c) value zero-padding: how many values fit in u32, leading-zero savings
// A watchdog aborts if RSS crosses 100 GiB (the load itself resides ~73 GiB).
//
// Usage: ckpt-mem-survey <ckpt.bin>
use cfx_minimal_mpt::State;
use std::error::Error;
use std::io::{self, BufReader, Read};
use std::time::Instant;

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

fn rss_kib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines().find_map(|l| {
                l.strip_prefix("VmRSS:")
                    .and_then(|v| v.split_whitespace().next())
                    .and_then(|kb| kb.parse::<u64>().ok())
            })
        })
        .unwrap_or(0)
}

fn gib(bytes: u64) -> f64 {
    bytes as f64 / (1u64 << 30) as f64
}

fn main() -> Result<(), Box<dyn Error>> {
    let ckpt = std::env::args().nth(1).expect("usage: ckpt-mem-survey <ckpt.bin>");

    // --- OOM watchdog: abort before the box does. ---
    std::thread::spawn(|| loop {
        if rss_kib() > 100 * 1024 * 1024 {
            eprintln!("WATCHDOG: RSS > 100 GiB, aborting to avoid OOM");
            std::process::abort();
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    });

    let file_len = std::fs::metadata(&ckpt)?.len();
    eprintln!("ckpt file: {} bytes ({:.2} GiB)", file_len, gib(file_len));

    let mut r = CountReader {
        inner: BufReader::with_capacity(1 << 22, std::fs::File::open(&ckpt)?),
        n: 0,
    };
    let (_ver, _height): (u32, u64) = bincode::deserialize_from(&mut r)?;
    let t = Instant::now();
    let state = State::from_reader(&mut r)?;
    eprintln!(
        "loaded in {:.1}s, RSS={:.2} GiB",
        t.elapsed().as_secs_f64(),
        gib(rss_kib() * 1024)
    );
    drop(r);

    let t = Instant::now();
    let s = state.memory_survey();
    eprintln!("survey pass: {:.1}s", t.elapsed().as_secs_f64());
    std::hint::black_box(&state);

    // ---------- raw measured sums ----------
    println!("\n==== RAW MEASURED (snapshot IncrementalTrie) ====");
    println!("n_entries              = {}", s.n_entries);
    println!("n_tombstone            = {}", s.n_tombstone);
    println!("entry_key_bytes (Σ)    = {} ({:.2} GiB)  [nibble keys]", s.entry_key_bytes, gib(s.entry_key_bytes));
    println!("value_bytes     (Σ)    = {} ({:.2} GiB)", s.value_bytes, gib(s.value_bytes));
    println!("n_cache                = {}", s.n_cache);
    println!("cache_key_bytes (Σ)    = {} ({:.2} GiB)  [nibble prefixes]", s.cache_key_bytes, gib(s.cache_key_bytes));
    println!("cache_val_bytes (H256) = {} ({:.2} GiB)  [32 * n_cache]", s.n_cache as u64 * 32, gib(s.n_cache as u64 * 32));

    // ---------- key byte-length histogram (non-empty buckets) ----------
    println!("\n==== ENTRY KEY BYTE-LENGTH HIST (len => count) ====");
    for (len, &c) in s.key_len_hist.iter().enumerate() {
        if c > 0 {
            println!("  {:>3}{} => {}", len, if len == 128 { "+" } else { " " }, c);
        }
    }

    // ---------- value length histogram ----------
    println!("\n==== VALUE LENGTH HIST (len => count) ====");
    for (len, &c) in s.val_len_hist.iter().enumerate() {
        if c > 0 {
            println!("  {:>3}{} => {}", len, if len == 33 { "+" } else { " " }, c);
        }
    }

    // ---------- (b) prefix-40-nibble dedup potential ----------
    let b_save = s.prefix40_stored_bytes - s.prefix40_deduped_bytes;
    println!("\n==== (b) FIRST-40-NIBBLE (20-byte) ROUTING-PREFIX DEDUP ====");
    println!("distinct 40-nibble prefixes = {}", s.distinct_prefix40);
    println!("prefix bytes stored now     = {} ({:.2} GiB)", s.prefix40_stored_bytes, gib(s.prefix40_stored_bytes));
    println!("prefix bytes if deduped     = {} ({:.2} GiB)", s.prefix40_deduped_bytes, gib(s.prefix40_deduped_bytes));
    println!("--> raw dedup savings       = {} ({:.2} GiB)   [ignores per-entry pointer overhead]", b_save, gib(b_save));

    // ---------- (c) value zero-padding potential ----------
    // strip leading zeros -> store only significant (big-endian) bytes
    let c_strip_lead = s.val_leading_zero_bytes;
    // u32-fitting values: currently `val_u32_fit_bytes`, could be <= 4 bytes each
    let c_u32_min = s.val_u32_fit_count * 4;
    let c_u32_save = s.val_u32_fit_bytes.saturating_sub(c_u32_min);
    println!("\n==== (c) VALUE ZERO-PADDING POTENTIAL ====");
    println!("Σ leading zero bytes        = {} ({:.2} GiB)  <- savings if store big-endian significant bytes only", c_strip_lead, gib(c_strip_lead));
    println!("Σ trailing zero bytes       = {} ({:.2} GiB)  <- (sanity: little-endian direction)", s.val_trailing_zero_bytes, gib(s.val_trailing_zero_bytes));
    println!("#values fitting u32 (<2^32) = {}  ({:.1}% of {})", s.val_u32_fit_count,
        100.0 * s.val_u32_fit_count as f64 / s.n_entries.max(1) as f64, s.n_entries);
    println!("  their current bytes       = {} ({:.2} GiB)", s.val_u32_fit_bytes, gib(s.val_u32_fit_bytes));
    println!("  if stored as u32 (4B)     = {} ({:.2} GiB)", c_u32_min, gib(c_u32_min));
    println!("--> raw u32-pack savings    = {} ({:.2} GiB)   [ignores overhead]", c_u32_save, gib(c_u32_save));

    // ---------- (a) inline pointer/alignment metadata (measured type sizes) ----------
    // BTreeMap<Vec<u8>, MptValue>: per slot = Vec<u8>(24) + MptValue(16) = 40B inline.
    let inline_entries = s.n_entries as u64 * 40;
    // cache FxHashMap<Vec<u8>, H256>: per occupied bucket = Vec<u8>(24)+H256(32)=56B.
    let inline_cache = s.n_cache as u64 * 56;
    println!("\n==== (a) INLINE POINTER/METADATA (measured type sizes) ====");
    println!("BTreeMap inline 40B/entry   = {} ({:.2} GiB)  [Vec<u8>24 + MptValue16, per SLOT — node fill<100% inflates this]", inline_entries, gib(inline_entries));
    println!("cache inline 56B/node       = {} ({:.2} GiB)  [Vec<u8>24 + H256 32, occupied buckets only]", inline_cache, gib(inline_cache));
    println!("(allocator per-alloc overhead & hashbrown empty buckets computed in write-up from these)");
    Ok(())
}
