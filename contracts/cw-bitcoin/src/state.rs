use bitcoin::Script;
use cosmwasm_std::{Order, Storage};
use cw_storage_plus::{Item, Map};

use crate::{
    adapter::Adapter,
    app::ConsensusKey,
    error::ContractResult,
    header::WorkHeader,
    interface::{BitcoinConfig, CheckpointConfig, DequeExtension, HeaderConfig, Validator, Xpub},
    recovery::RecoveryTx,
};

/// TODO: store in smart contract
pub const CHECKPOINT_CONFIG: Item<CheckpointConfig> = Item::new("checkpoint_config");
pub const HEADER_CONFIG: Item<HeaderConfig> = Item::new("header");
pub const BITCOIN_CONFIG: Item<BitcoinConfig> = Item::new("bitcoin_config");

/// The recovery scripts for nBTC account holders, which are users' desired
/// destinations for BTC to be paid out to in the emergency disbursal
/// process if the network is halted.
/// Mapping validator Address => bitcoin::Script
pub const RECOVERY_SCRIPTS: Map<&str, Adapter<bitcoin::Script>> = Map::new("recovery_scripts");

pub const VALIDATORS: Map<&ConsensusKey, u64> = Map::new("validators");

/// Mapping validator Address => ConsensusKey
pub const SIGNERS: Map<&str, ConsensusKey> = Map::new("signers");

// by_cons Map<ConsensusKey, Xpub>
pub const SIG_KEYS: Map<&ConsensusKey, Xpub> = Map::new("sig_keys");

/// The collection also includes an set of all signatory extended public keys,
/// which is used to prevent duplicate keys from being submitted.
/// xpubs Map<Xpub::encode(), ()>
pub const XPUBS: Map<&[u8], ()> = Map::new("xpubs");

/// A queue of Bitcoin block headers, along with the total estimated amount of
/// work (measured in hashes) done in the headers included in the queue.
///
/// The header queue is used to validate headers as they are received from the
/// Bitcoin network, ensuring each header is associated with a valid
/// proof-of-work and that the chain of headers is valid.
///
/// The queue is able to reorg if a new chain of headers is received that
/// contains more work than the current chain, however it can not process reorgs
/// that are deeper than the length of the queue (the length will be at the
/// configured pruning level based on the `max_length` config parameter).
pub const HEADERS: DequeExtension<WorkHeader> = DequeExtension::new("headers");

pub const RECOVERY_TXS: DequeExtension<RecoveryTx> = DequeExtension::new("recovery_txs");

/// A queue of outpoints to expire, sorted by expiration timestamp.
pub const EXPIRATION_QUEUE: Map<(u64, &str), ()> = Map::new("expiration_queue");

/// A set of outpoints.
pub const OUTPOINTS: Map<&str, ()> = Map::new("outpoints");

pub const CHECKPOINT_QUEUE_ID: Item<u64> = Item::new("cp_queue_id");

pub const CHECKPOINT_QUEUE_ID_PREFIX: &str = "checkpoint";

pub fn next_checkpoint_queue_id(store: &mut dyn Storage) -> ContractResult<u64> {
    let id: u64 = last_checkpoint_queue_id(store)? + 1;
    CHECKPOINT_QUEUE_ID.save(store, &id)?;
    Ok(id)
}

pub fn last_checkpoint_queue_id(store: &dyn Storage) -> ContractResult<u64> {
    Ok(CHECKPOINT_QUEUE_ID.may_load(store)?.unwrap_or_default())
}

pub fn to_output_script(store: &dyn Storage, dest: &str) -> ContractResult<Option<Script>> {
    Ok(RECOVERY_SCRIPTS
        .load(store, dest)
        .ok()
        .map(|script| script.into_inner()))
}

pub fn get_validators(store: &dyn Storage) -> ContractResult<Vec<Validator>> {
    VALIDATORS
        .range(store, None, None, Order::Ascending)
        .map(|item| {
            let (k, v) = item?;
            Ok(Validator {
                power: v,
                pubkey: k,
            })
        })
        .collect()
}