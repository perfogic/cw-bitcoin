use bitcoin::BlockHash;
use cosmwasm_std::{ContractResult, Storage};
use std::str::FromStr;

use crate::{
    app::Bitcoin,
    error::{ContractError, ContractResult},
    state::{header_height, HEADERS, HEADER_CONFIG},
};

pub fn query_header_height(store: &dyn Storage) -> ContractResult<u32> {
    header_height(store)
}

pub fn query_deposit_fees(store: &dyn Storage, index: Option<u32>) -> ContractResult<u64> {
    let header_config = HEADER_CONFIG.load(store)?;
    let btc = Bitcoin::new(header_config);
    let checkpoint = btc.get_checkpoint(store, index)?;
    let input_vsize = checkpoint.sigset.est_witness_vsize() + 40;
    let deposit_fees = btc.calc_minimum_deposit_fees(input_vsize, checkpoint.fee_rate);
    Ok(deposit_fees)
}

pub fn query_withdrawal_fees(
    store: &dyn Storage,
    address: String,
    index: Option<u32>,
) -> ContractResult<u64> {
    let header_config = HEADER_CONFIG.load(store)?;
    let btc = Bitcoin::new(header_config);
    let checkpoint = btc.get_checkpoint(store, index)?;
    let btc_address = bitcoin::Address::from_str(address.as_str())
        .map_err(|err| ContractError::App(err.to_string()))?;
    let script = btc_address.script_pubkey();
    let withdrawal_fees =
        btc.calc_minimum_withdrawal_fees(script.len() as u64, checkpoint.fee_rate);
    Ok(withdrawal_fees)
}

pub fn query_sidechain_block_hash(store: &dyn Storage) -> ContractResult<BlockHash> {
    // let hash = app_client(&self.app_client_addr)
    //     .query(|app: InnerApp| Ok(app.bitcoin.headers.hash()?))
    //     .await?;
    // let hash = BlockHash::from_slice(hash.as_slice())?;
    // Ok(hash)
    let header_config = HEADER_CONFIG.load(store)?;
    let btc = Bitcoin::new(header_config);
    let hash = btc.headers.hash(store)?;
    Ok(hash)
}

pub fn query_checkpoint_by_index(store: &dyn Storage, index: u32) -> ContractResult<u64> {
    let header_config = HEADER_CONFIG.load(store)?;
    let btc = Bitcoin::new(header_config);
    let checkpoint = btc.get_checkpoint(store, Some(index))?;
    Ok(checkpoint.height)
}