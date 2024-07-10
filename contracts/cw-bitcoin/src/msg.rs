use cosmwasm_schema::{cw_serde, QueryResponses};
use cosmwasm_std::Binary;

use crate::interface::Dest;

#[cw_serde]
pub struct InstantiateMsg {}

#[cw_serde]
pub enum ExecuteMsg {
    RelayDeposit {
        btc_tx: Binary,
        btc_height: u32,
        btc_proof: Binary,
        btc_vout: u32,
        sigset_index: u32,
        dest: Dest,
    },
}

#[cw_serde]
#[derive(QueryResponses)]
pub enum QueryMsg {
    #[returns(u64)]
    DepositFees { index: Option<u32> },
    #[returns(u64)]
    WithdrawalFees { address: String, index: Option<u32> },
}

#[cw_serde]
pub struct MigrateMsg {}
