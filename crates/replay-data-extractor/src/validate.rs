use anyhow::{ensure, Result};
use cfx_types::{Space, H256};
use cfxpack::{
    decode::decode_packet,
    packet::{Block, Packet, FLAG_HAS_TRANSACTIONS, FLAG_PIVOT},
    verify::{verify_packet, VerifyReport},
};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayReport {
    pub packet_bytes: usize,
    pub epoch_count: usize,
    pub block_count: usize,
    pub transaction_count: usize,
    pub duplicate_transaction_count: usize,
    pub native_transaction_count: usize,
    pub espace_transaction_count: usize,
    pub first_block_number: u64,
    pub last_block_number: u64,
}

pub type ReplayValidationReport = ReplayReport;

pub fn validate_replay_packet(data: &[u8]) -> Result<ReplayReport> {
    let verify = verify_packet(data)?;
    let input = decode_packet(data)?;
    validate_replay_input(&input, &verify)
}

pub fn validate_replay_input(input: &Packet, verify: &VerifyReport) -> Result<ReplayReport> {
    ensure!(!input.blocks.is_empty(), "replay packet has no blocks");
    ensure!(
        input.blocks.len() == verify.block_count as usize,
        "decoded block count differs from packet verifier"
    );

    let mut expected_index = 0usize;
    let mut epoch_count = 0usize;
    let mut group_start = 0usize;
    let mut block_number = input.first_block_number;
    let mut tx_count = 0usize;
    let mut native_tx_count = 0usize;
    let mut espace_tx_count = 0usize;
    let mut duplicate_tx_count = 0usize;
    let mut seen_tx_hashes = HashMap::<H256, usize>::new();

    while group_start < input.blocks.len() {
        let Some(relative_pivot) = input.blocks[group_start..]
            .iter()
            .position(|block| block.flags & FLAG_PIVOT != 0)
        else {
            anyhow::bail!("epoch group has no pivot block");
        };
        let pivot_index = group_start + relative_pivot;
        let epoch = input.blocks[pivot_index].height;
        ensure!(
            input.blocks[pivot_index].epoch == epoch,
            "pivot block epoch does not match pivot height"
        );
        for block in &input.blocks[group_start..=pivot_index] {
            ensure!(
                block.index == expected_index,
                "block index is not sequential"
            );
            ensure!(
                block.flags & 0b1100_0000 == 0,
                "reserved block flag bits must be zero"
            );
            ensure!(
                block.epoch == epoch,
                "block epoch does not match its pivot epoch"
            );
            let block_has_transactions = !block.transactions.is_empty();
            ensure!(
                (block.flags & FLAG_HAS_TRANSACTIONS) != 0 || !block_has_transactions,
                "transaction flag does not match decoded transaction list"
            );
            validate_block_transactions(
                block,
                block_number,
                &mut tx_count,
                &mut native_tx_count,
                &mut espace_tx_count,
                &mut duplicate_tx_count,
                &mut seen_tx_hashes,
            )?;
            expected_index += 1;
            block_number += 1;
        }
        epoch_count += 1;
        group_start = pivot_index + 1;
    }

    ensure!(
        tx_count == verify.transaction_items as usize,
        "decoded transaction count differs from packet verifier"
    );

    Ok(ReplayReport {
        packet_bytes: verify.packet_bytes,
        epoch_count,
        block_count: input.blocks.len(),
        transaction_count: tx_count,
        duplicate_transaction_count: duplicate_tx_count,
        native_transaction_count: native_tx_count,
        espace_transaction_count: espace_tx_count,
        first_block_number: input.first_block_number,
        last_block_number: block_number - 1,
    })
}

fn validate_block_transactions(
    block: &Block,
    _block_number: u64,
    tx_count: &mut usize,
    native_tx_count: &mut usize,
    espace_tx_count: &mut usize,
    duplicate_tx_count: &mut usize,
    seen_tx_hashes: &mut HashMap<H256, usize>,
) -> Result<()> {
    for tx in &block.transactions {
        *tx_count += 1;
        match tx.space() {
            Space::Native => *native_tx_count += 1,
            Space::Ethereum => *espace_tx_count += 1,
        }
        if seen_tx_hashes.insert(tx.hash(), block.index).is_some() {
            *duplicate_tx_count += 1;
        }
    }
    Ok(())
}
