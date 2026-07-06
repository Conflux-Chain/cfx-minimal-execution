use cfxpack::packet::{FLAG_PIVOT, FLAG_SKIPPED_EXECUTION};
use cfxpack::{container, decode};

// Reproduce the EXACT runtime decode+group path: same (decompressed) file, same
// pos_reference_enable_height as the running config, same filter(SKIPPED)+pivot
// walk as consensus::execute_input. Args: <epoch> <raw_file> [pos_h].
fn main() -> anyhow::Result<()> {
    let target_epoch: u64 = std::env::args().nth(1).expect("usage: <epoch> <raw_file> [pos_h]").parse()?;
    let path = std::env::args().nth(2).expect("usage: <epoch> <raw_file> [pos_h]");
    let pos_h: u64 = std::env::args().nth(3).map(|s| s.parse()).transpose()?.unwrap_or(37_400_000);

    let raw = std::fs::read(&path)?;

    let directory = container::parse_directory(&raw)?;
    for (start, count, offset, length) in &directory {
        let end = start + count - 1;
        if target_epoch < *start || target_epoch > end {
            continue;
        }
        eprintln!("group {}-{} pos_h={}", start, end, pos_h);
        let packet = decode::decode_packet_ext(&raw[*offset..*offset + *length], pos_h)?;

        // Same as consensus::execute_input: strip SKIPPED, then walk pivots.
        let blocks: Vec<_> = packet
            .blocks
            .iter()
            .filter(|b| b.flags & FLAG_SKIPPED_EXECUTION == 0)
            .collect();
        let mut start_idx = 0usize;
        while start_idx < blocks.len() {
            let Some(rel) = blocks[start_idx..].iter().position(|b| b.flags & FLAG_PIVOT != 0) else {
                break;
            };
            let end_idx = start_idx + rel;
            let epoch_blocks = &blocks[start_idx..=end_idx];
            let pivot = epoch_blocks.last().unwrap();
            if pivot.height == target_epoch {
                println!(
                    "RUNTIME-GROUP epoch(pivot.height)={} pivot.epoch={} blocks={}",
                    pivot.height,
                    pivot.epoch,
                    epoch_blocks.len()
                );
                for (i, b) in epoch_blocks.iter().enumerate() {
                    println!(
                        "  block[{}] hash={:?} height={} epoch={} flags=0x{:02x} txs={} \
                         base_price_core=0x{:x} base_price_espace=0x{:x} gas_limit=0x{:x}",
                        i, b.hash, b.height, b.epoch, b.flags, b.transactions.len(),
                        b.base_price_core, b.base_price_espace, b.gas_limit
                    );
                    for (j, tx) in b.transactions.iter().enumerate() {
                        println!(
                            "    tx[{}] recomputed_hash={:?} space={:?} type_id={} chain_id={:?} nonce={} gas=0x{:x} gas_price=0x{:x} action={:?} value={}",
                            j, tx.hash(), tx.space(), tx.type_id(), tx.chain_id(), tx.nonce(), tx.gas(), tx.gas_price(), tx.action(), tx.value()
                        );
                    }
                }
                return Ok(());
            }
            start_idx = end_idx + 1;
        }
        println!("epoch {} pivot not found in group", target_epoch);
        return Ok(());
    }
    eprintln!("epoch {} not in any group", target_epoch);
    Ok(())
}
