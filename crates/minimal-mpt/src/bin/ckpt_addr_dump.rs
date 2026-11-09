// Dump the effective (layered) state of a set of accounts from a v2 replay
// checkpoint streamed on stdin (already decompressed). Layering: delta >
// intermediate > snapshot; tombstones remove. Output: one line per live
// canonical key, "hex(canonical_key)\thex(value)".
//
// usage: zstdcat ckpt.bin.zst | ckpt_addr_dump <addr-file>
// addr-file: one 40-hex-char eSpace address per line.

use cfx_minimal_mpt::StorageKeyWithSpace;
use std::collections::{BTreeMap, HashSet};
use std::io::{self, BufReader, Read, Write};

fn read_u64_le(r: &mut impl Read) -> io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

fn read_u32_le(r: &mut impl Read) -> io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn read_lp(r: &mut impl Read, buf: &mut Vec<u8>) -> io::Result<()> {
    let len = read_u64_le(r)? as usize;
    buf.clear();
    buf.resize(len, 0);
    r.read_exact(buf)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn main() -> io::Result<()> {
    let addr_file = std::env::args().nth(1).expect("usage: ckpt_addr_dump <addr-file>");
    let addrs: HashSet<[u8; 20]> = std::fs::read_to_string(&addr_file)?
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let h = l.trim().trim_start_matches("0x");
            let mut a = [0u8; 20];
            for i in 0..20 {
                a[i] = u8::from_str_radix(&h[2 * i..2 * i + 2], 16).unwrap();
            }
            a
        })
        .collect();
    eprintln!("filtering {} addresses", addrs.len());

    let stdin = io::stdin();
    let mut r = BufReader::with_capacity(16 << 20, stdin.lock());

    let version = read_u32_le(&mut r)?;
    assert_eq!(version, 2, "unexpected checkpoint version {version}");
    let height = read_u64_le(&mut r)?;
    eprintln!("checkpoint height={height}");

    // Effective state: canonical key -> Some(value) | None (tombstoned).
    let mut eff: BTreeMap<Vec<u8>, Option<Vec<u8>>> = BTreeMap::new();

    let addr_of_canonical = |key: &[u8]| -> Option<[u8; 20]> {
        if key.len() >= 20 {
            let mut a = [0u8; 20];
            a.copy_from_slice(&key[..20]);
            Some(a)
        } else {
            None
        }
    };

    // --- snapshot ---
    let snap_count = read_u64_le(&mut r)? as usize;
    eprintln!("snapshot entries: {snap_count}");
    let mut key = Vec::new();
    let mut val = Vec::new();
    for _ in 0..snap_count {
        read_lp(&mut r, &mut key)?;
        read_lp(&mut r, &mut val)?;
        if let Some(a) = addr_of_canonical(&key) {
            if addrs.contains(&a) {
                eff.insert(key.clone(), Some(val.clone()));
            }
        }
    }

    // --- intermediate, then delta ---
    for layer in ["intermediate", "delta"] {
        let count = read_u64_le(&mut r)? as usize;
        eprintln!("{layer} entries: {count}");
        for _ in 0..count {
            read_lp(&mut r, &mut key)?;
            let tag = read_u32_le(&mut r)?;
            let value = match tag {
                0 => {
                    read_lp(&mut r, &mut val)?;
                    Some(val.clone())
                }
                1 => None,
                t => panic!("bad MptValue tag {t}"),
            };
            let Ok(decoded) = StorageKeyWithSpace::from_delta_mpt_key(&key) else {
                continue;
            };
            let Ok(canonical) = decoded.to_key_bytes() else {
                continue;
            };
            if let Some(a) = addr_of_canonical(&canonical) {
                if addrs.contains(&a) {
                    eff.insert(canonical, value);
                }
            }
        }
    }

    let stdout = io::stdout();
    let mut w = io::BufWriter::new(stdout.lock());
    let mut live = 0usize;
    for (k, v) in &eff {
        if let Some(v) = v {
            writeln!(w, "{}\t{}", hex(k), hex(v))?;
            live += 1;
        }
    }
    w.flush()?;
    eprintln!("done: {live} live entries for filtered addresses");
    Ok(())
}
