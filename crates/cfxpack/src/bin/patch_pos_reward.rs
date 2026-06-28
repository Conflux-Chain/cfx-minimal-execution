use anyhow::{anyhow, Context, Result};
use cfxpack::container::parse_directory;
use cfxpack::decode::decode_packet_ext;
use cfxpack::packet::{encode_packet, PosRewardAccount, PosRewardEntry, FLAG_PIVOT};
use cfx_types::{Address, H256, U256};
use std::env;
use std::fs;

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("scan") => scan(&args[2..]),
        Some("inject") => inject(&args[2..]),
        _ => {
            eprintln!("Usage:");
            eprintln!("  patch_pos_reward scan <file.cfxpack> [hash]");
            eprintln!("  patch_pos_reward inject <file.cfxpack> <json> <to_epoch>");
            Ok(())
        }
    }
}

fn scan(args: &[String]) -> Result<()> {
    let path = args.first().ok_or_else(|| anyhow!("missing file path"))?;
    let filter_hash: Option<H256> = args.get(1).map(|s| {
        s.strip_prefix("0x")
            .unwrap_or(s)
            .parse()
            .expect("parse filter hash")
    });
    let data = fs::read(path).context("read cfxpack file")?;
    let entries = parse_directory(&data)?;
    let pos_ref_height = 37_400_000u64;

    for (start, _count, offset, length) in &entries {
        let packet_data = &data[*offset..*offset + *length];
        let packet = decode_packet_ext(packet_data, pos_ref_height)
            .with_context(|| format!("decode group starting at epoch {start}"))?;
        for block in &packet.blocks {
            let dominated = filter_hash.map_or(
                !block.pos_rewards.is_empty() || !block.unlock_events.is_empty(),
                |h| block.hash == h,
            );
            if dominated {
                let is_pivot = block.flags & FLAG_PIVOT != 0;
                println!(
                    "epoch={} height={} hash={:?} pivot={} pos_rewards={} unlock_events={} txs={} pos_view={:?}",
                    block.epoch, block.height, block.hash, is_pivot,
                    block.pos_rewards.len(), block.unlock_events.len(),
                    block.transactions.len(), block.pos_view,
                );
                for pr in &block.pos_rewards {
                    println!("  reward: exec_hash={:?} accounts={}", pr.execution_epoch_hash, pr.account_rewards.len());
                    for a in &pr.account_rewards {
                        println!("    addr={:?} id={:?} reward={}", a.address, a.pos_identifier, a.reward);
                    }
                }
            }
        }
    }
    Ok(())
}

fn inject(args: &[String]) -> Result<()> {
    let cfxpack_path = args.get(0).ok_or_else(|| anyhow!("missing cfxpack path"))?;
    let json_path = args.get(1).ok_or_else(|| anyhow!("missing json path"))?;
    let to_epoch: u64 = args.get(2).ok_or_else(|| anyhow!("missing to_epoch"))?.parse().context("parse to_epoch")?;

    let json_str = fs::read_to_string(json_path).context("read JSON file")?;
    let reward_entry = parse_reward_json(&json_str)?;
    println!("Loaded {} account rewards from JSON", reward_entry.account_rewards.len());

    let data = fs::read(cfxpack_path).context("read cfxpack file")?;
    let entries = parse_directory(&data)?;
    let pos_ref_height = 37_400_000u64;

    let header_and_dir_len = entries.first().map(|e| e.2).unwrap_or(data.len());
    let mut out = data[..header_and_dir_len].to_vec();
    let mut modified = false;

    for (i, (start, _count, offset, length)) in entries.iter().enumerate() {
        let packet_data = &data[*offset..*offset + *length];
        let mut packet = decode_packet_ext(packet_data, pos_ref_height)
            .with_context(|| format!("decode group starting at epoch {start}"))?;

        if let Some(pivot) = packet.blocks.iter_mut().find(|b| b.epoch == to_epoch && b.flags & FLAG_PIVOT != 0) {
            let mut entry = reward_entry.clone();
            println!("Injecting reward into epoch={} pivot hash={:?}", to_epoch, pivot.hash);
            entry.execution_epoch_hash = pivot.hash;
            pivot.pos_rewards.push(entry);
            modified = true;
        }

        let new_packet_data = encode_packet(&packet)
            .with_context(|| format!("re-encode group starting at epoch {start}"))?;
        let new_offset = out.len();
        let new_length = new_packet_data.len();
        out.extend_from_slice(&new_packet_data);

        let dir_entry_offset = 24 + i * 32;
        out[dir_entry_offset + 16..dir_entry_offset + 24].copy_from_slice(&(new_offset as u64).to_le_bytes());
        out[dir_entry_offset + 24..dir_entry_offset + 32].copy_from_slice(&(new_length as u64).to_le_bytes());
    }

    if modified {
        let out_path = format!("{}.patched", cfxpack_path);
        fs::write(&out_path, &out).context("write patched file")?;
        println!("Wrote patched file to {}", out_path);

        let verify_data = fs::read(&out_path).context("re-read patched file")?;
        let verify_entries = parse_directory(&verify_data)?;
        for (start, _count, offset, length) in &verify_entries {
            let p = &verify_data[*offset..*offset + *length];
            let packet = decode_packet_ext(p, pos_ref_height)
                .with_context(|| format!("verify group starting at epoch {start}"))?;
            for block in &packet.blocks {
                if block.epoch == to_epoch && block.flags & FLAG_PIVOT != 0 {
                    println!("Verified: epoch={} pos_rewards={} accounts={}",
                        block.epoch, block.pos_rewards.len(),
                        block.pos_rewards.iter().map(|r| r.account_rewards.len()).sum::<usize>());
                }
            }
        }
    } else {
        println!("Target epoch {} pivot not found in file — nothing patched.", to_epoch);
    }
    Ok(())
}

fn parse_reward_json(json: &str) -> Result<PosRewardEntry> {
    let mut account_rewards = Vec::new();
    let mut exec_hash = H256::zero();

    for line in json.lines() {
        let line = line.trim();
        if line.contains("\"execution_epoch_hash\"") {
            if let Some(hash_str) = extract_json_string(line, "execution_epoch_hash") {
                exec_hash = hash_str.strip_prefix("0x").unwrap_or(&hash_str)
                    .parse().context("parse execution_epoch_hash")?;
            }
        }
        if line.contains("\"address\"") && line.contains("\"pos_identifier\"") && line.contains("\"reward\"") {
            let addr_str = extract_json_string(line, "address").ok_or_else(|| anyhow!("missing address"))?;
            let id_str = extract_json_string(line, "pos_identifier").ok_or_else(|| anyhow!("missing pos_identifier"))?;
            let reward_str = extract_json_string(line, "reward").ok_or_else(|| anyhow!("missing reward"))?;

            let address: Address = addr_str.strip_prefix("0x").unwrap_or(&addr_str)
                .parse().context("parse address")?;
            let pos_identifier: H256 = id_str.strip_prefix("0x").unwrap_or(&id_str)
                .parse().context("parse pos_identifier")?;
            let reward = U256::from_dec_str(&reward_str).context("parse reward")?;

            account_rewards.push(PosRewardAccount { address, pos_identifier, reward });
        }
    }

    Ok(PosRewardEntry { account_rewards, execution_epoch_hash: exec_hash })
}

fn extract_json_string(line: &str, key: &str) -> Option<String> {
    let pattern = format!("\"{}\":", key);
    let pos = line.find(&pattern)?;
    let rest = &line[pos + pattern.len()..];
    let rest = rest.trim().trim_start_matches('"');
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}
