use cosmwasm_std::to_json_vec;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tsify::Tsify;

use crate::error::ContractResult;

#[derive(Clone, Debug, PartialOrd, PartialEq, Eq, Ord, Deserialize, Serialize, Tsify)]
#[tsify(into_wasm_abi, from_wasm_abi)]
pub struct IbcDest {
    pub source_port: String,
    pub source_channel: String,
    #[serde(skip)]
    pub receiver: String,
    #[serde(skip)]
    pub sender: String,
    pub timeout_timestamp: u64,
    pub memo: String,
}

#[derive(Clone, Debug, PartialOrd, PartialEq, Eq, Ord, Deserialize, Serialize, Tsify)]
#[tsify(into_wasm_abi, from_wasm_abi)]
pub enum Dest {
    Address(String),
    Ibc(IbcDest),
}

impl Dest {
    pub fn to_receiver_addr(&self) -> String {
        match self {
            Self::Address(addr) => addr.to_string(),
            Self::Ibc(dest) => dest.receiver.to_string(),
        }
    }

    pub fn to_source_addr(&self) -> String {
        match self {
            Self::Address(addr) => addr.to_string(),
            Self::Ibc(dest) => dest.sender.to_string(),
        }
    }

    pub fn commitment_bytes(&self) -> ContractResult<Vec<u8>> {
        let bytes = match self {
            Self::Address(addr) => addr.as_bytes().into(),
            Self::Ibc(dest) => Sha256::digest(to_json_vec(dest).unwrap()).to_vec(),
        };

        Ok(bytes)
    }
}

#[wasm_bindgen::prelude::wasm_bindgen]
pub fn commitmentBytes(dest: Dest) -> Vec<u8> {
    dest.commitment_bytes().unwrap()
}
