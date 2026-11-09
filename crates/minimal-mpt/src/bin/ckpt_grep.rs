// Stream-scan a v2 replay checkpoint (on stdin, already decompressed) for a
// byte pattern in every KV value across all three layers. Reports the decoded
// storage key (layer, space, kind, address) of each hit, plus whether a
// snapshot/intermediate hit is overridden by a higher layer.
//
// usage: zstdcat ckpt.bin.zst | ckpt_grep <hex-needle>

use cfx_minimal_mpt::{StorageKey, StorageKeyWithSpace};
use std::collections::HashSet;
use std::io::{self, BufReader, Read};

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

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    if hay.len() < needle.len() {
        return false;
    }
    let first = needle[0];
    let mut i = 0;
    while i + needle.len() <= hay.len() {
        if hay[i] == first && &hay[i..i + needle.len()] == needle {
            return true;
        }
        i += 1;
    }
    false
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn describe(k: &StorageKeyWithSpace) -> String {
    let space = format!("{:?}", k.space);
    match &k.key {
        StorageKey::Account(a) => format!("{space}\taccount\t0x{}\t-", hex(a)),
        StorageKey::StorageRoot(a) => format!("{space}\tstorage-root\t0x{}\t-", hex(a)),
        StorageKey::Storage { address, storage_key } => {
            format!("{space}\tstorage\t0x{}\tslot=0x{}", hex(address), hex(storage_key))
        }
        StorageKey::CodeRoot(a) => format!("{space}\tcode-root\t0x{}\t-", hex(a)),
        StorageKey::Code { address, code_hash } => {
            format!("{space}\tcode\t0x{}\thash=0x{}", hex(address), hex(code_hash))
        }
        StorageKey::DepositList(a) => format!("{space}\tdeposit\t0x{}\t-", hex(a)),
        StorageKey::VoteList(a) => format!("{space}\tvote\t0x{}\t-", hex(a)),
        other => format!("{space}\t{other:?}\t-\t-"),
    }
}

fn main() -> io::Result<()> {
    let needle_hex = std::env::args().nth(1).expect("usage: ckpt_grep <hex-needle>");
    let needle_hex = needle_hex.trim_start_matches("0x").to_lowercase();
    let needle: Vec<u8> = (0..needle_hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&needle_hex[i..i + 2], 16).unwrap())
        .collect();

    let stdin = io::stdin();
    let mut r = BufReader::with_capacity(16 << 20, stdin.lock());

    let version = read_u32_le(&mut r)?;
    assert_eq!(version, 2, "unexpected checkpoint version {version}");
    let height = read_u64_le(&mut r)?;
    eprintln!("checkpoint height={height}");

    // Canonical key bytes of every hit in a lower layer, to detect overrides.
    let mut hit_canonical: HashSet<Vec<u8>> = HashSet::new();
    let mut hits = 0usize;

    // --- snapshot ---
    let snap_count = read_u64_le(&mut r)? as usize;
    eprintln!("snapshot entries: {snap_count}");
    let mut key = Vec::new();
    let mut val = Vec::new();
    for i in 0..snap_count {
        read_lp(&mut r, &mut key)?;
        read_lp(&mut r, &mut val)?;
        if contains(&val, &needle) {
            hits += 1;
            match StorageKeyWithSpace::from_key_bytes(&key) {
                Ok(k) => {
                    println!("snapshot\t{}\tval_len={}", describe(&k), val.len());
                    hit_canonical.insert(key.clone());
                }
                Err(e) => println!("snapshot\t<undecodable {e}>\tkey=0x{}", hex(&key)),
            }
        }
        if i % 50_000_000 == 0 && i > 0 {
            eprintln!("  ... {i} snapshot entries scanned");
        }
    }

    // --- intermediate, then delta: bincode BTreeMap<Box<[u8]>, MptValue> ---
    for layer in ["intermediate", "delta"] {
        let count = read_u64_le(&mut r)? as usize;
        eprintln!("{layer} entries: {count}");
        for _ in 0..count {
            read_lp(&mut r, &mut key)?;
            let tag = read_u32_le(&mut r)?;
            let is_some = match tag {
                0 => {
                    read_lp(&mut r, &mut val)?;
                    true
                }
                1 => false,
                t => panic!("bad MptValue tag {t}"),
            };
            let decoded = StorageKeyWithSpace::from_delta_mpt_key(&key);
            let canonical = decoded.as_ref().ok().and_then(|k| k.to_key_bytes().ok());
            if is_some && contains(&val, &needle) {
                hits += 1;
                match &decoded {
                    Ok(k) => println!("{layer}\t{}\tval_len={}", describe(k), val.len()),
                    Err(e) => println!("{layer}\t<undecodable {e}>\tkey=0x{}", hex(&key)),
                }
                if let Some(c) = &canonical {
                    hit_canonical.insert(c.clone());
                }
            } else if let Some(c) = &canonical {
                if hit_canonical.contains(c) {
                    let k = decoded.as_ref().unwrap();
                    let what = if is_some { "overridden-no-match" } else { "tombstoned" };
                    println!("{layer}\tOVERRIDE({what})\t{}", describe(k));
                }
            }
        }
    }

    eprintln!("done: {hits} matching values");
    Ok(())
}
