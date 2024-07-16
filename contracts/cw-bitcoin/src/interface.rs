use bitcoin::util::bip32::ExtendedPubKey;
use bitcoin::BlockHeader;
use cosmwasm_schema::cw_serde;
use cosmwasm_schema::schemars::JsonSchema;
use cosmwasm_schema::serde::{de, ser, Deserialize, Serialize};
use cosmwasm_std::from_slice;
use cosmwasm_std::to_vec;
use cosmwasm_std::Addr;
use cosmwasm_std::Binary;
use cosmwasm_std::Coin;
use cosmwasm_std::Storage;
use cosmwasm_std::Uint128;
use cw_storage_plus::Deque;
use derive_more::{Deref, DerefMut};
use sha2::{Digest, Sha256};

use crate::adapter::Adapter;
use crate::app::ConsensusKey;
use crate::app::NETWORK;
use crate::constants::MAX_CHECKPOINT_AGE;
use crate::constants::MAX_CHECKPOINT_INTERVAL;
use crate::constants::MAX_DEPOSIT_AGE;
use crate::constants::MAX_FEE_RATE;
use crate::constants::MAX_LENGTH;
use crate::constants::MAX_TARGET;
use crate::constants::MAX_TIME_INCREASE;
use crate::constants::MIN_DEPOSIT_AMOUNT;
use crate::constants::MIN_FEE_RATE;
use crate::constants::MIN_WITHDRAWAL_AMOUNT;
use crate::constants::RETARGET_INTERVAL;
use crate::constants::SIGSET_THRESHOLD;
use crate::constants::TARGET_SPACING;
use crate::constants::TARGET_TIMESPAN;
use crate::constants::TRANSFER_FEE;
use crate::constants::USER_FEE_FACTOR;
use crate::error::ContractError;
use crate::error::ContractResult;
use crate::header::WorkHeader;
use crate::header::WrappedHeader;

#[derive(Deref, DerefMut)]
pub struct DequeExtension<'a, T> {
    // prefix of the deque items
    key_prefix: [u8; 2],
    namespace: &'a [u8],
    // see https://doc.rust-lang.org/std/marker/struct.PhantomData.html#unused-type-parameters for why this is needed
    #[deref]
    #[deref_mut]
    queue: Deque<'a, T>,
}

impl<'a, T: Serialize + de::DeserializeOwned> DequeExtension<'a, T> {
    pub const fn new(prefix: &'a str) -> Self {
        Self {
            namespace: prefix.as_bytes(),
            key_prefix: (prefix.len() as u16).to_be_bytes(),
            queue: Deque::new(prefix),
        }
    }

    pub fn clear(&self, store: &mut dyn Storage) -> ContractResult<()> {
        let mut queue_len = self.len(store)?;

        while queue_len > 0 {
            self.pop_back(store)?;
            queue_len -= 1;
        }
        Ok(())
    }

    pub fn get_key(&self, pos: u32) -> Vec<u8> {
        let key = &pos.to_be_bytes();
        let size = self.namespace.len() + 2 + key.len();
        let mut out = Vec::with_capacity(size);
        out.extend_from_slice(&self.key_prefix);
        out.extend_from_slice(self.namespace);
        out.extend_from_slice(key);
        out
    }

    /// Sets the value at the given position in the queue. Returns [`StdError::NotFound`] if index is out of bounds
    pub fn set(&self, storage: &mut dyn Storage, pos: u32, value: &T) -> ContractResult<()> {
        let prefixed_key = self.get_key(pos);
        storage.set(&prefixed_key, &to_vec(value)?);
        Ok(())
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(crate = "cosmwasm_schema::serde")]
pub struct Accounts {
    transfers_allowed: bool,
    transfer_exceptions: Vec<String>,
    accounts: Vec<(String, Coin)>,
}

impl Accounts {
    pub fn balance(&self, address: String) -> Option<Coin> {
        self.accounts
            .iter()
            .find(|item| item.0 == address)
            .map(|item| item.1.clone())
    }

    pub fn withdraw(&mut self, address: Addr, amount: Uint128) -> ContractResult<Coin> {
        if let Some((_, coin)) = self
            .accounts
            .iter_mut()
            .find(|acc| acc.0.eq(address.as_str()))
        {
            coin.amount.checked_sub(amount)?;
            return Ok(coin.clone());
        }
        Err(ContractError::Coins("Insufficient funds".into()))
    }
}

#[cw_serde]
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

#[cw_serde]
pub enum Dest {
    Address(Addr),
    Ibc(IbcDest),
}

impl Dest {
    pub fn to_receiver_addr(&self) -> String {
        match self {
            Self::Address(addr) => addr.to_string(),
            Self::Ibc(dest) => dest.receiver.to_string(),
        }
    }

    pub fn commitment_bytes(&self) -> ContractResult<Vec<u8>> {
        let bytes = match self {
            Self::Address(addr) => addr.as_bytes().into(),
            Self::Ibc(dest) => Sha256::digest(dest.receiver.as_bytes()).to_vec(),
        };

        Ok(bytes)
    }
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(crate = "cosmwasm_schema::serde")]
pub struct Validator {
    pub pubkey: ConsensusKey,
    pub power: u64,
}

#[cw_serde]
pub struct BitcoinConfig {
    /// The minimum number of checkpoints that must be produced before
    /// withdrawals are enabled.
    pub min_withdrawal_checkpoints: u32,
    /// The minimum amount of BTC a deposit must send to be honored, in
    /// satoshis.
    pub min_deposit_amount: u64,
    /// The minimum amount of BTC a withdrawal must withdraw, in satoshis.
    pub min_withdrawal_amount: u64,
    /// TODO: remove this, not used
    pub max_withdrawal_amount: u64,
    /// The maximum length of a withdrawal output script, in bytes.
    pub max_withdrawal_script_length: u64,
    /// The fee charged for an nBTC transfer, in micro-satoshis.
    pub transfer_fee: u64,
    /// The minimum number of confirmations a Bitcoin block must have before it
    /// is considered finalized. Note that in the current implementation, the
    /// actual number of confirmations required is `min_confirmations + 1`.
    pub min_confirmations: u32,
    /// The number which amounts in satoshis are multiplied by to get the number
    /// of units held in nBTC accounts. In other words, the amount of
    /// subdivisions of satoshis which nBTC accounting uses.
    pub units_per_sat: u64,

    // (These fields were moved to `checkpoint::Config`)
    pub emergency_disbursal_min_tx_amt: u64,

    pub emergency_disbursal_lock_time_interval: u32,

    pub emergency_disbursal_max_tx_size: u64,

    /// If a signer does not submit signatures for this many consecutive
    /// checkpoints, they are considered offline and are removed from the
    /// signatory set (jailed) and slashed.    
    pub max_offline_checkpoints: u32,
    /// The minimum number of confirmations a checkpoint must have on the
    /// Bitcoin network before it is considered confirmed. Note that in the
    /// current implementation, the actual number of confirmations required is
    /// `min_checkpoint_confirmations + 1`.    
    pub min_checkpoint_confirmations: u32,
    /// The maximum amount of BTC that can be held in the network, in satoshis.    
    pub capacity_limit: u64,

    pub max_deposit_age: u64,

    pub fee_pool_target_balance: u64,

    pub fee_pool_reward_split: (u64, u64),
}

impl BitcoinConfig {
    fn bitcoin() -> Self {
        Self {
            min_withdrawal_checkpoints: 4,
            min_deposit_amount: MIN_DEPOSIT_AMOUNT,
            min_withdrawal_amount: MIN_WITHDRAWAL_AMOUNT,
            max_withdrawal_amount: 64,
            max_withdrawal_script_length: 64,
            transfer_fee: TRANSFER_FEE,
            min_confirmations: 1,
            units_per_sat: 1_000_000,
            max_offline_checkpoints: 20,
            min_checkpoint_confirmations: 0,
            capacity_limit: 21 * 100_000_000,     // 21 BTC
            max_deposit_age: MAX_DEPOSIT_AGE, // 2 weeks. Initially there may not be many deposits & withdraws
            fee_pool_target_balance: 100_000_000, // 1 BTC
            fee_pool_reward_split: (1, 10),
            emergency_disbursal_min_tx_amt: 1000,
            emergency_disbursal_lock_time_interval: 60 * 60 * 24 * 7 * 8, // 8 weeks
            emergency_disbursal_max_tx_size: 50_000,
        }
    }
}

impl Default for BitcoinConfig {
    fn default() -> Self {
        match NETWORK {
            bitcoin::Network::Testnet | bitcoin::Network::Bitcoin => Self::bitcoin(),
            _ => unimplemented!(),
        }
    }
}

/// Configuration parameters used in processing checkpoints.
#[cw_serde]
pub struct CheckpointConfig {
    /// The minimum amount of time between the creation of checkpoints, in
    /// seconds.
    ///
    /// If a checkpoint is to be created, but less than this time has passed
    /// since the last checkpoint was created (in the `Building` state), the
    /// current `Building` checkpoint will be delayed in advancing to `Signing`.
    pub min_checkpoint_interval: u64,

    /// The maximum amount of time between the creation of checkpoints, in
    /// seconds.
    ///
    /// If a checkpoint would otherwise not be created, but this amount of time
    /// has passed since the last checkpoint was created (in the `Building`
    /// state), the current `Building` checkpoint will be advanced to `Signing`
    /// and a new `Building` checkpoint will be added.
    pub max_checkpoint_interval: u64,

    /// The maximum number of inputs allowed in a checkpoint transaction.
    ///
    /// This is used to prevent the checkpoint transaction from being too large
    /// to be accepted by the Bitcoin network.
    ///
    /// If a checkpoint has more inputs than this when advancing from `Building`
    /// to `Signing`, the excess inputs will be moved to the suceeding,
    /// newly-created `Building` checkpoint.
    pub max_inputs: u64,

    /// The maximum number of outputs allowed in a checkpoint transaction.
    ///
    /// This is used to prevent the checkpoint transaction from being too large
    /// to be accepted by the Bitcoin network.
    ///
    /// If a checkpoint has more outputs than this when advancing from `Building`
    /// to `Signing`, the excess outputs will be moved to the suceeding,
    /// newly-created `Building` checkpoint.∑
    pub max_outputs: u64,

    /// The default fee rate to use when creating the first checkpoint of the
    /// network, in satoshis per virtual byte.    
    pub fee_rate: u64,

    /// The maximum age of a checkpoint to retain, in seconds.
    ///
    /// Checkpoints older than this will be pruned from the state, down to a
    /// minimum of 10 checkpoints in the checkpoint queue.
    pub max_age: u64,

    /// The number of blocks to target for confirmation of the checkpoint
    /// transaction.
    ///
    /// This is used to adjust the fee rate of the checkpoint transaction, to
    /// ensure it is confirmed within the target number of blocks. The fee rate
    /// will be adjusted up if the checkpoint transaction is not confirmed
    /// within the target number of blocks, and will be adjusted down if the
    /// checkpoint transaction faster than the target.    
    pub target_checkpoint_inclusion: u32,

    /// The lower bound to use when adjusting the fee rate of the checkpoint
    /// transaction, in satoshis per virtual byte.    
    pub min_fee_rate: u64,

    /// The upper bound to use when adjusting the fee rate of the checkpoint
    /// transaction, in satoshis per virtual byte.    
    pub max_fee_rate: u64,

    /// The value (in basis points) to multiply by when calculating the miner
    /// fee to deduct from a user's deposit or withdrawal. This value should be
    /// at least 1 (10,000 basis points).
    ///
    /// The difference in the fee deducted and the fee paid in the checkpoint
    /// transaction is added to the fee pool, to help the network pay for
    /// its own miner fees.    
    pub user_fee_factor: u64,

    /// The threshold of signatures required to spend reserve scripts, as a
    /// ratio represented by a tuple, `(numerator, denominator)`.
    ///
    /// For example, `(9, 10)` means the threshold is 90% of the signatory set.    
    pub sigset_threshold: (u64, u64),

    /// The minimum amount of nBTC an account must hold to be eligible for an
    /// output in the emergency disbursal.    
    pub emergency_disbursal_min_tx_amt: u64,

    /// The amount of time between the creation of a checkpoint and when the
    /// associated emergency disbursal transactions can be spent, in seconds.    
    pub emergency_disbursal_lock_time_interval: u32,

    /// The maximum size of a final emergency disbursal transaction, in virtual
    /// bytes.
    ///
    /// The outputs to be included in final emergency disbursal transactions
    /// will be distributed across multiple transactions around this size.    
    pub emergency_disbursal_max_tx_size: u64,

    /// The maximum number of unconfirmed checkpoints before the network will
    /// stop creating new checkpoints.
    ///
    /// If there is a long chain of unconfirmed checkpoints, there is possibly
    /// an issue causing the transactions to not be included on Bitcoin (e.g. an
    /// invalid transaction was created, the fee rate is too low even after
    /// adjustments, Bitcoin miners are censoring the transactions, etc.), in
    /// which case the network should evaluate and fix the issue before creating
    /// more checkpoints.
    ///
    /// This will also stop the fee rate from being adjusted too high if the
    /// issue is simply with relayers failing to report the confirmation of the
    /// checkpoint transactions.    
    pub max_unconfirmed_checkpoints: u32,
}

impl Default for CheckpointConfig {
    fn default() -> Self {
        Self {
            min_checkpoint_interval: 60 * 5,
            max_checkpoint_interval: MAX_CHECKPOINT_INTERVAL,
            max_inputs: 40,
            max_outputs: 200,
            max_age: MAX_CHECKPOINT_AGE,
            target_checkpoint_inclusion: 2,
            min_fee_rate: MIN_FEE_RATE, // relay threshold is 1 sat/vbyte
            max_fee_rate: MAX_FEE_RATE,
            user_fee_factor: USER_FEE_FACTOR, // 2.7x
            sigset_threshold: SIGSET_THRESHOLD,
            emergency_disbursal_min_tx_amt: 1000,
            emergency_disbursal_lock_time_interval: 60 * 60 * 24 * 7 * 8, // 8 weeks
            emergency_disbursal_max_tx_size: 50_000,
            max_unconfirmed_checkpoints: 15,
            fee_rate: 0,
        }
    }
}

/// A Bitcoin extended public key, used to derive Bitcoin public keys which
/// signatories sign transactions with.
#[derive(Copy, Clone, PartialEq, Deref, Eq, Debug, PartialOrd, Ord, Hash)]
pub struct Xpub {
    pub key: ExtendedPubKey,
}

impl Xpub {
    /// Creates a new `Xpub` from an `ExtendedPubKey`.
    pub fn new(key: ExtendedPubKey) -> Self {
        Xpub { key }
    }

    /// Gets the `ExtendedPubKey` from the `Xpub`.
    pub fn inner(&self) -> &ExtendedPubKey {
        &self.key
    }
}

impl From<ExtendedPubKey> for Xpub {
    fn from(key: ExtendedPubKey) -> Self {
        Xpub { key }
    }
}

impl From<&ExtendedPubKey> for Xpub {
    fn from(key: &ExtendedPubKey) -> Self {
        Xpub { key: *key }
    }
}

/// Serializes as a string
impl Serialize for Xpub {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: ser::Serializer,
    {
        let dest = self.key.encode();
        Binary::from(dest).serialize(serializer)
    }
}

/// Deserializes as string
impl<'de> Deserialize<'de> for Xpub {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        let v = Binary::deserialize(deserializer)?;
        let inner = ExtendedPubKey::decode(v.as_slice()).map_err(de::Error::custom)?;
        Ok(inner.into())
    }
}

///  HeaderConfiguration parameters for Bitcoin header processing.
// TODO: implement trait that returns constants for bitcoin::Network variants
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(crate = "cosmwasm_schema::serde")]
#[schemars(crate = "cosmwasm_schema::schemars")]
pub struct HeaderConfig {
    /// The maximum number of headers that can be stored in the header queue
    /// before pruning.
    pub max_length: u64,
    /// The maximum amount of time (in seconds) that can pass between the
    /// timestamp of the last header in the header queue and the timestamp of
    /// the next header to be added.
    pub max_time_increase: u32,
    /// The height of the trusted header.
    pub trusted_height: u32,
    /// The interval (in blocks) at which the difficulty target is adjusted.
    pub retarget_interval: u32,
    /// The target time interval (in seconds) between blocks.
    pub target_spacing: u32,
    /// The target amount of time (in seconds) that should pass between the
    /// timestamps of the first and last header in a retargeting period. This
    /// should be equivalent to `retarget_interval * target_spacing`.
    // TODO: derive from `retarget_interval` and `target_spacing`
    pub target_timespan: u32,
    /// The maximum target value.
    pub max_target: u32,
    /// Whether or not the header queue should retarget difficulty.
    pub retargeting: bool,
    /// Whether or not the header queue should drop back down to the minimum
    /// difficulty after a certain amount of time has passed (used in Bitcoin
    /// testnet).
    pub min_difficulty_blocks: bool,
    /// The trusted header (the header which populates the queue when it is
    /// newly created), as encoded bytes.
    pub trusted_header: Adapter<BlockHeader>,
}

impl HeaderConfig {
    pub fn from_bytes(checkpoint_json: &[u8]) -> ContractResult<Self> {
        let checkpoint: (u32, BlockHeader) = from_slice(checkpoint_json)?;
        let (height, header) = checkpoint;

        Ok(Self {
            max_length: MAX_LENGTH,
            max_time_increase: MAX_TIME_INCREASE,
            trusted_height: height,
            retarget_interval: RETARGET_INTERVAL,
            target_spacing: TARGET_SPACING,
            target_timespan: TARGET_TIMESPAN,
            max_target: MAX_TARGET,
            trusted_header: header.into(),
            retargeting: true,
            min_difficulty_blocks: false,
        })
    }

    pub fn work_header(&self) -> WorkHeader {
        let decoded_adapter: Adapter<BlockHeader> = self.trusted_header.into();
        let wrapped_header = WrappedHeader::new(decoded_adapter, self.trusted_height);
        let work_header = WorkHeader::new(wrapped_header.clone(), wrapped_header.work());
        work_header
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(crate = "cosmwasm_schema::serde")]
pub struct ChangeRates {
    pub withdrawal: u16,
    pub sigset_change: u16,
}
