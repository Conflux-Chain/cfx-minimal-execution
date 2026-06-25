//! Transaction-segment decoding: expand the per-block RLP payload back into
//! `SignedTransaction`s, resolving duplicate references and the interned
//! gas-price / address / sender-nonce tables.

use super::{decode_u256_bytes, table_get};
use crate::codec::{read_uleb128, zigzag_decode};
use crate::packet::{PosRewardAccount, PosRewardEntry, SenderBaseNonce, FLAG_TX_COMPRESSED};
use anyhow::{anyhow, ensure, Context, Result};
use cfx_types::{Address, AddressSpaceUtil, H256, U256};
use primitives::{
    transaction::{
        Cip1559Transaction, Cip2930Transaction, Eip1559Transaction, Eip155Transaction,
        Eip2930Transaction, Eip7702Transaction, EthereumTransaction, NativeTransaction,
        TypedNativeTransaction,
    },
    AccessListItem, Action, AuthorizationListItem, SignedTransaction,
};
use rlp::Rlp;
use snap::raw::Decoder as SnapDecoder;

#[allow(clippy::too_many_arguments)]
pub(super) fn decode_tx_payload(
    data: &[u8],
    tx_segment_offset: usize,
    tx_offset_units: u64,
    flags: u8,
    block_epoch: u64,
    addresses: &[Address],
    gas_prices: &[U256],
    sender_base_nonces: &[SenderBaseNonce],
    previous_blocks: &[Vec<SignedTransaction>],
) -> Result<(
    Vec<SignedTransaction>,
    Vec<Option<(usize, usize)>>,
    Vec<PosRewardEntry>,
)> {
    let absolute = tx_segment_offset + tx_offset_units as usize * 4;
    ensure!(absolute < data.len(), "tx payload offset out of bounds");
    let mut offset = absolute;
    let payload_len = read_uleb128(data, &mut offset)? as usize;
    ensure!(
        offset + payload_len <= data.len(),
        "tx payload exceeds packet"
    );
    let payload = &data[offset..offset + payload_len];
    let decoded = if flags & FLAG_TX_COMPRESSED != 0 {
        SnapDecoder::new()
            .decompress_vec(payload)
            .context("snappy-decompress tx payload")?
    } else {
        payload.to_vec()
    };
    let rlp = Rlp::new(&decoded);
    let mut out = Vec::new();
    let mut refs = Vec::new();
    let mut pos_rewards = Vec::new();
    for i in 0..rlp.item_count()? {
        let item = rlp.at(i)?;
        let marker: u8 = item.val_at(0)?;
        if marker == 1 {
            let block_index: usize = item.val_at::<u64>(1)? as usize;
            let tx_index: usize = item.val_at::<u64>(2)? as usize;
            out.push(
                previous_blocks
                    .get(block_index)
                    .and_then(|b| b.get(tx_index))
                    .ok_or_else(|| anyhow!("duplicate tx reference out of range"))?
                    .clone(),
            );
            refs.push(Some((block_index, tx_index)));
        } else if marker == 3 {
            // PoS interest distribution (tag 3), not a transaction — collected
            // separately and applied at epoch settlement. See encode/append_pos_reward.
            pos_rewards.push(decode_pos_reward(&item)?);
        } else {
            out.push(decode_tx_item(
                &item,
                block_epoch,
                addresses,
                gas_prices,
                sender_base_nonces,
            )?);
            refs.push(None);
        }
    }
    Ok((out, refs, pos_rewards))
}

/// Decode a tag-3 PoS-reward item: `[3, [[address, pos_identifier, reward], ...],
/// execution_epoch_hash]`. Mirrors production `PosRewardInfo`.
fn decode_pos_reward(item: &Rlp) -> Result<PosRewardEntry> {
    let rewards = item.at(1)?;
    let mut account_rewards = Vec::with_capacity(rewards.item_count()?);
    for j in 0..rewards.item_count()? {
        let r = rewards.at(j)?;
        let addr_bytes = r.val_at::<Vec<u8>>(0)?;
        ensure!(addr_bytes.len() == 20, "pos reward address not 20 bytes");
        let id_bytes = r.val_at::<Vec<u8>>(1)?;
        ensure!(id_bytes.len() == 32, "pos reward identifier not 32 bytes");
        account_rewards.push(PosRewardAccount {
            address: Address::from_slice(&addr_bytes),
            pos_identifier: H256::from_slice(&id_bytes),
            reward: decode_u256_bytes(r.val_at::<Vec<u8>>(2)?)?,
        });
    }
    let exec_bytes = item.val_at::<Vec<u8>>(2)?;
    ensure!(exec_bytes.len() == 32, "execution_epoch_hash not 32 bytes");
    Ok(PosRewardEntry {
        account_rewards,
        execution_epoch_hash: H256::from_slice(&exec_bytes),
    })
}

fn decode_tx_item(
    item: &Rlp,
    block_epoch: u64,
    addresses: &[Address],
    gas_prices: &[U256],
    sender_base_nonces: &[SenderBaseNonce],
) -> Result<SignedTransaction> {
    let space_marker: u8 = item.val_at(0)?;
    let type_id: u64 = item.val_at(1)?;
    let sender_index = item.val_at::<u64>(2)? as usize;
    let sender = table_get(addresses, sender_index)?;
    let base_nonce = sender_base_nonces
        .iter()
        .find(|entry| entry.sender_index == sender_index)
        .map(|entry| entry.base_nonce)
        .unwrap_or(0);
    let nonce = U256::from(base_nonce + item.val_at::<u64>(3)?);
    let gas_price = decode_price(&item.at(4)?, gas_prices)?;
    let priority_price = decode_price(&item.at(5)?, gas_prices)?;
    let gas = U256::from(item.val_at::<u64>(6)?);
    let action = decode_action(&item.at(7)?, addresses)?;
    let value = decode_u256_bytes(item.val_at::<Vec<u8>>(8)?)?;

    match space_marker {
        0 => decode_native_tx(
            item,
            type_id,
            sender,
            block_epoch,
            nonce,
            gas_price,
            priority_price,
            gas,
            action,
            value,
        ),
        2 => decode_ethereum_tx(
            item,
            type_id,
            sender,
            nonce,
            gas_price,
            priority_price,
            gas,
            action,
            value,
        ),
        _ => Err(anyhow!("unsupported tx space marker {space_marker}")),
    }
}

#[allow(clippy::too_many_arguments)]
fn decode_native_tx(
    item: &Rlp,
    type_id: u64,
    sender: Address,
    block_epoch: u64,
    nonce: U256,
    gas_price: U256,
    priority_price: U256,
    gas: U256,
    action: Action,
    value: U256,
) -> Result<SignedTransaction> {
    let storage_limit = item.val_at(9)?;
    let epoch_delta = zigzag_decode(item.val_at::<u64>(10)?);
    let data = item.val_at::<Vec<u8>>(11)?.into();
    let access_list = optional_list::<AccessListItem>(item, 12)?;
    let epoch_height = (block_epoch as i64).wrapping_add(epoch_delta) as u64;
    let tx = match type_id {
        0 => TypedNativeTransaction::Cip155(NativeTransaction {
            nonce,
            gas_price,
            gas,
            action,
            value,
            storage_limit,
            epoch_height,
            chain_id: 0,
            data,
        }),
        1 => TypedNativeTransaction::Cip2930(Cip2930Transaction {
            nonce,
            gas_price,
            gas,
            action,
            value,
            storage_limit,
            epoch_height,
            chain_id: 0,
            data,
            access_list,
        }),
        2 => TypedNativeTransaction::Cip1559(Cip1559Transaction {
            nonce,
            max_priority_fee_per_gas: priority_price,
            max_fee_per_gas: gas_price,
            gas,
            action,
            value,
            storage_limit,
            epoch_height,
            chain_id: 0,
            data,
            access_list,
        }),
        _ => return Err(anyhow!("unsupported native tx type {type_id}")),
    };
    Ok(tx.fake_sign_rpc(sender.with_native_space()))
}

#[allow(clippy::too_many_arguments)]
fn decode_ethereum_tx(
    item: &Rlp,
    type_id: u64,
    sender: Address,
    nonce: U256,
    gas_price: U256,
    priority_price: U256,
    gas: U256,
    action: Action,
    value: U256,
) -> Result<SignedTransaction> {
    let data = item.val_at::<Vec<u8>>(9)?.into();
    let access_list = optional_list::<AccessListItem>(item, 10)?;
    let tx = match type_id {
        0 => EthereumTransaction::Eip155(Eip155Transaction {
            nonce,
            gas_price,
            gas,
            action,
            value,
            chain_id: None,
            data,
        }),
        1 => EthereumTransaction::Eip2930(Eip2930Transaction {
            chain_id: 0,
            nonce,
            gas_price,
            gas,
            action,
            value,
            data,
            access_list,
        }),
        2 => EthereumTransaction::Eip1559(Eip1559Transaction {
            chain_id: 0,
            nonce,
            max_priority_fee_per_gas: priority_price,
            max_fee_per_gas: gas_price,
            gas,
            action,
            value,
            data,
            access_list,
        }),
        4 => EthereumTransaction::Eip7702(Eip7702Transaction {
            chain_id: 0,
            nonce,
            max_priority_fee_per_gas: priority_price,
            max_fee_per_gas: gas_price,
            gas,
            destination: match action {
                Action::Call(address) => address,
                Action::Create => Address::zero(),
            },
            value,
            data,
            access_list,
            authorization_list: optional_list::<AuthorizationListItem>(item, 11)?,
        }),
        _ => return Err(anyhow!("unsupported ethereum tx type {type_id}")),
    };
    Ok(tx.fake_sign_rpc(sender.with_evm_space()))
}

fn decode_price(item: &Rlp, gas_prices: &[U256]) -> Result<U256> {
    let mode: u8 = item.val_at(0)?;
    match mode {
        0 => table_get(gas_prices, item.val_at::<u64>(1)? as usize),
        1 => decode_u256_bytes(item.val_at::<Vec<u8>>(1)?),
        _ => Err(anyhow!("invalid gas price mode {mode}")),
    }
}

fn decode_action(item: &Rlp, addresses: &[Address]) -> Result<Action> {
    let mode: u8 = item.val_at(0)?;
    match mode {
        0 => Ok(Action::Create),
        1 => Ok(Action::Call(table_get(
            addresses,
            item.val_at::<u64>(1)? as usize,
        )?)),
        _ => Err(anyhow!("invalid action mode {mode}")),
    }
}

fn optional_list<T: rlp::Decodable>(item: &Rlp, index: usize) -> Result<Vec<T>> {
    if item.item_count()? > index {
        item.list_at(index).map_err(Into::into)
    } else {
        Ok(Vec::new())
    }
}
