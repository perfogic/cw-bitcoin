use super::{
    signatory::SignatorySet,
    threshold_sig::{Signature, ThresholdSig},
};
use crate::{
    adapter::Adapter,
    interface::{Accounts, BitcoinConfig, CheckpointConfig, Dest, Xpub},
    state::{
        next_checkpoint_queue_id, to_output_script, CHECKPOINT_QUEUE_ID_PREFIX, RECOVERY_SCRIPTS,
    },
};
use crate::{
    constants::DEFAULT_FEE_RATE,
    error::{ContractError, ContractResult},
};
use crate::{interface::DequeExtension, signatory::derive_pubkey};
use bitcoin::{blockdata::transaction::EcdsaSighashType, Sequence, Transaction, TxIn, TxOut};
use bitcoin::{hashes::Hash, Script};
use cosmwasm_schema::cw_serde;
use cosmwasm_std::{Coin, Env, Order, Storage};
use derive_more::{Deref, DerefMut};
use serde::{Deserialize, Serialize};

// use std::convert::TryFrom;
use std::ops::{Deref, DerefMut};

/// The status of a checkpoint. Checkpoints start as `Building`, and eventually
/// advance through the three states.
#[cw_serde]
#[derive(Default)]
pub enum CheckpointStatus {
    #[default]
    /// The checkpoint is being constructed. It can still be mutated by adding
    /// bitcoin inputs and outputs, pending actions, etc.    
    Building,

    /// The inputs in the checkpoint are being signed. The checkpoint's
    /// structure is frozen in this stage, and it is no longer valid to add or
    /// remove inputs or outputs.
    Signing,

    /// All inputs in the the checkpoint are fully signed and the contained
    /// checkpoint transaction is valid and ready to be broadcast on the bitcoin
    /// network.
    Complete,
}

/// An input to a Bitcoin transaction - possibly in an unsigned state.
///
/// This structure contains the necessary data for signing an input, and once
/// signed can be turned into a `bitcoin::TxIn` for inclusion in a Bitcoin
/// transaction.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct Input {
    /// The outpoint being spent by this input.
    pub prevout: Adapter<bitcoin::OutPoint>,

    /// The script of the output being spent by this input. In practice, this
    /// will be a pay-to-witness-script-hash (P2WSH) script, containing the hash
    /// of the script in the `redeem_script` field.
    pub script_pubkey: Adapter<bitcoin::Script>,

    /// The redeem script which `script_pubkey` contains the hash of, supplied
    /// in the witness of the input when spending. In practice, this will
    /// represent a multisig tied to the associated signatory set.
    pub redeem_script: Adapter<bitcoin::Script>,

    /// The index of the signatory set which this input is associated with.
    pub sigset_index: u32,

    /// Bytes representing a commitment to a destination (e.g. a native nomic
    /// account address, an IBC transfer destination, or a 0-byte for the
    /// reserve output owned by the network). These bytes are included in the
    /// redeem script to tie the funds to the destination.
    pub dest: Vec<u8>,

    /// The amount of the input being spent, in satoshis.
    pub amount: u64,

    /// An estimate of the size of the witness for this input, in virtual bytes.
    /// This size is used for fee calculations.
    pub est_witness_vsize: u64,

    /// The signatures for this input. This structure is where the signatories
    /// coordinate to submit their signatures, and starts out with no
    /// signatures.
    pub signatures: ThresholdSig,
}

impl Input {
    /// Converts the `Input` to a `bitcoin::TxIn`, useful when constructing an
    /// actual Bitcoin transaction to be broadcast.
    pub fn to_txin(&self) -> ContractResult<TxIn> {
        let mut witness = self.signatures.to_witness()?;
        if self.signatures.signed() {
            witness.push(self.redeem_script.to_bytes());
        }

        Ok(bitcoin::TxIn {
            previous_output: *self.prevout,
            script_sig: bitcoin::Script::new(),
            sequence: Sequence(u32::MAX),
            witness: bitcoin::Witness::from_vec(witness),
        })
    }

    /// Creates an `Input` which spends the given Bitcoin outpoint, populating
    /// it with an empty signing state to be signed by the given signatory set.
    pub fn new(
        prevout: bitcoin::OutPoint,
        sigset: &SignatorySet,
        dest: &[u8],
        amount: u64,
        threshold: (u64, u64),
    ) -> ContractResult<Self> {
        let script_pubkey = sigset.output_script(dest, threshold)?;
        let redeem_script = sigset.redeem_script(dest, threshold)?;

        Ok(Input {
            prevout: Adapter::new(prevout),
            script_pubkey: Adapter::new(script_pubkey),
            redeem_script: Adapter::new(redeem_script),
            sigset_index: sigset.index(),
            dest: dest.to_vec(),
            amount,
            est_witness_vsize: sigset.est_witness_vsize(),
            signatures: ThresholdSig::from_sigset(sigset),
        })
    }

    /// The estimated size of the input, including the worst-case size of the
    /// witness once fully signed, in virtual bytes.
    pub fn est_vsize(&self) -> u64 {
        self.est_witness_vsize + 40
    }
}

/// A bitcoin transaction output, wrapped to implement the core `orga` traits.
pub type Output = Adapter<bitcoin::TxOut>;

/// A bitcoin transaction, as a native `orga` data structure.
#[derive(Default, Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct BitcoinTx {
    /// The locktime field included in the bitcoin transaction, representing
    /// either a block height or timestamp.
    pub lock_time: u32,

    /// A counter representing how many inputs have been fully-signed so far.
    /// The transaction is valid and ready to be broadcast to the bitcoin
    /// network once all inputs have been signed.
    pub signed_inputs: u16,

    /// The inputs to the transaction.
    pub input: Vec<Input>,

    /// The outputs to the transaction.
    pub output: Vec<Output>,
}

impl BitcoinTx {
    /// Converts the `BitcoinTx` to a `bitcoin::Transaction`.
    pub fn to_bitcoin_tx(&self) -> ContractResult<Transaction> {
        Ok(bitcoin::Transaction {
            version: 1,
            lock_time: bitcoin::PackedLockTime(self.lock_time),
            input: self
                .input
                .iter()
                .map(|input| input.to_txin())
                .collect::<ContractResult<_>>()?,
            output: self
                .output
                .iter()
                .map(|output| output.clone().into_inner())
                .collect(),
        })
    }

    /// Creates a new `BitcoinTx` with the given locktime, and no inputs or
    /// outputs.
    pub fn with_lock_time(lock_time: u32) -> Self {
        BitcoinTx {
            lock_time,
            ..Default::default()
        }
    }

    /// Returns `true` if all inputs in the transaction are fully signed,
    /// otherwise returns `false`.
    pub fn signed(&self) -> bool {
        self.signed_inputs as usize == self.input.len()
    }

    /// The estimated size of the transaction, including the worst-case sizes of
    /// all input witnesses once fully signed, in virtual bytes.
    pub fn vsize(&self) -> ContractResult<u64> {
        Ok(self.to_bitcoin_tx()?.vsize().try_into()?)
    }

    /// The hash of the transaction. Note that this will change if any inputs or
    /// outputs are added, removed, or modified, so should only be used once the
    /// transaction is known to be final.
    pub fn txid(&self) -> ContractResult<bitcoin::Txid> {
        let bitcoin_tx = self.to_bitcoin_tx()?;
        Ok(bitcoin_tx.txid())
    }

    /// The total value of the outputs in the transaction, in satoshis.
    pub fn value(&self) -> ContractResult<u64> {
        self.output
            .iter()
            .fold(Ok(0), |sum: ContractResult<u64>, out| Ok(sum? + out.value))
    }

    /// Calculates the sighash to be signed for the given input index, and
    /// populates the input's signing state with it. This should be used when a
    /// transaction is finalized and its structure will not change, and
    /// coordination of signing will begin.
    pub fn populate_input_sig_message(&mut self, input_index: usize) -> ContractResult<()> {
        let bitcoin_tx = self.to_bitcoin_tx()?;
        let mut sc = bitcoin::util::sighash::SighashCache::new(&bitcoin_tx);
        let input = self
            .input
            .get_mut(input_index)
            .ok_or(ContractError::InputIndexOutOfBounds(input_index))?;

        let sighash = sc.segwit_signature_hash(
            input_index,
            &input.redeem_script,
            input.amount,
            EcdsaSighashType::All,
        )?;

        input.signatures.set_message(sighash.into_inner());

        Ok(())
    }

    /// Deducts the given amount of satoshis evenly from all outputs in the
    /// transaction, leaving the difference as the amount to be paid to miners
    /// as a fee.
    ///
    /// This function will fail if the fee is greater than the value of the
    /// outputs in the transaction. Any inputs which are not large enough to pay
    /// their share of the fee will be removed.
    pub fn deduct_fee(&mut self, fee: u64) -> ContractResult<()> {
        if fee == 0 {
            return Ok(());
        }

        if self.output.is_empty() {
            // TODO: Bitcoin module error
            return Err(ContractError::BitcoinFee(fee));
        }

        let mut output_len = self.output.len() as u64;

        // This algorithm calculates the amount to attempt to deduct from each
        // output (`threshold`), and then removes any outputs which are too
        // small to pay this. Since removing outputs changes the threshold,
        // additional iterations will be required until all remaining outputs
        // are large enough.
        let threshold = loop {
            // The threshold is the fee divided by the number of outputs (each
            // output pays an equal share of the fee).
            let threshold = fee / output_len;

            // Remove any outputs which are too small to pay the threshold.
            let mut min_output = u64::MAX;
            self.output = self
                .output
                .clone()
                .into_iter()
                .filter(|output| {
                    let dust_value = output.script_pubkey.dust_value().to_sat();
                    let adjusted_output = output.value.saturating_sub(dust_value);
                    if adjusted_output < min_output {
                        min_output = adjusted_output;
                    }
                    adjusted_output > threshold
                })
                .collect();

            output_len = self.output.len() as u64;

            // Handle the case where no outputs remain.
            if output_len == 0 {
                break threshold;
            }

            // If the threshold is less than the smallest output, we can stop
            // here.
            let threshold = fee / output_len;
            if min_output >= threshold {
                break threshold;
            }
        };

        // Deduct the final fee share from each remaining output.
        for i in 0..output_len {
            let output = self.output.get_mut(i as usize).unwrap();
            output.value -= threshold;
        }

        Ok(())
    }
}

/// `BatchType` represents one of the three types of transaction batches in a
/// checkpoint.
#[derive(Debug)]
pub enum BatchType {
    /// The batch containing the "final emergency disbursal transactions".
    ///
    /// This batch will contain at least one and potentially many transactions,
    /// paying out to the recipients of the emergency disbursal (e.g. recovery
    /// wallets of nBTC holders).
    Disbursal,

    /// The batch containing the intermediate transaction.
    ///
    /// This batch will always contain exactly one transaction, the
    /// "intermediate emergency disbursal transaction", which spends the reserve
    /// output of a stuck checkpoint transaction, and pays out to inputs which
    /// will be spent by the final emergency disbursal transactions.
    IntermediateTx,

    /// The batch containing the checkpoint transaction. This batch will always
    /// contain exactly one transaction, the "checkpoint transaction".
    ///
    /// This transaction spends the reserve output of the previous checkpoint
    /// transaction and the outputs of any incoming deposits. It pays out to the
    /// the latest signatory set (in the "reserve output") and to destinations
    /// of any requested withdrawals.
    Checkpoint,
}

/// A batch of transactions in a checkpoint.
///
/// A batch is a collection of transactions which are atomically signed
/// together. Signatories submit signatures for all inputs in all transactions
/// in the batch at once. Once the batch is fully signed, the checkpoint can
/// advance to signing of the next batch, if any.
#[derive(Default, Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct Batch {
    signed_txs: u16,
    batch: Vec<BitcoinTx>,
}

impl Deref for Batch {
    type Target = Vec<BitcoinTx>;

    fn deref(&self) -> &Self::Target {
        &self.batch
    }
}

impl DerefMut for Batch {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.batch
    }
}

impl Batch {
    fn signed(&self) -> bool {
        self.signed_txs as usize == self.batch.len()
    }
}

/// `Checkpoint` is the main structure which coordinates the network's
/// management of funds on the Bitcoin blockchain.
///
/// The network periodically creates checkpoints, which are Bitcoin transactions
/// that move the funds held in reserve. There is a singular sequential chain of
/// checkpoints, and each checkpoint has an associated signatory set. The
/// signatory set is a list of public keys of the signers performing the
/// decentralized custody of the funds held in reserve.
///
/// Checkpoints are each associated with a main transaction, the "checkpoint
/// transaction", which spends the reserve output of the previous checkpoint
/// transaction and the outputs of any incoming deposits. It pays out to the the
/// latest signatory set (in the "reserve output") and to destinations of any
/// requested withdrawals. This transaction is included in the third batch of
/// the `batches` deque.
///
/// Checkpoints are also associated with a set of transactions which pay out to
/// the recipients of the emergency disbursal (e.g. recovery wallets of nBTC
/// holders), if the checkpoint transaction is not spent after a given amount of
/// time (e.g. two weeks). These transactions are broken up into a single
/// "intermediate emergency disbursal transaction" (in the second batch of the
/// `batches` deque), and one or more "final emergency disbursal transactions"
/// (in the first batch of the `batches` deque).
#[derive(Default, Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct Checkpoint {
    /// The status of the checkpoint, either `Building`, `Signing`, or
    /// `Complete`.
    pub status: CheckpointStatus,

    /// Pending transfers of nBTC to be processed once the checkpoint is fully
    /// signed. These transfers are processed in lockstep with the checkpointing
    /// process in order to keep nBTC balances in sync with the emergency
    /// disbursal.
    ///
    /// These transfers can be initiated by a simple nBTC send or by a deposit.    
    pub pending: Vec<(String, Coin)>,

    /// The batches of transactions in the checkpoint, to each be signed
    /// atomically, in order. The first batch contains the "final emergency
    /// disbursal transactions", the second batch contains the "intermediate
    /// emergency disbursal transaction", and the third batch contains the
    /// "checkpoint transaction".
    pub batches: Vec<Batch>,

    /// The fee rate to use when calculating the miner fee for the transactions
    /// in the checkpoint, in satoshis per virtual byte.
    ///
    /// This rate is automatically adjusted per-checkpoint, being increased when
    /// completed checkpoints are not being confirmed on the Bitcoin network
    /// faster than the target confirmation speed (implying the network is
    /// paying too low of a fee), and being decreased if checkpoints are
    /// confirmed faster than the target confirmation speed.    
    pub fee_rate: u64,

    /// The height of the Bitcoin block at which the checkpoint was fully signed
    /// and ready to be broadcast to the Bitcoin network, used by the fee
    /// adjustment algorithm to determine if the checkpoint was confirmed too
    /// fast or too slow.    
    pub signed_at_btc_height: Option<u32>,

    /// Whether or not to honor relayed deposits made against this signatory
    /// set. This can be used, for example, to enforce a cap on deposits into
    /// the system.    
    pub deposits_enabled: bool,

    pub fees_collected: u64,

    /// The signatory set associated with the checkpoint. Note that deposits to
    /// slightly older signatory sets can still be processed in this checkpoint,
    /// but the reserve output will be paid to the latest signatory set.
    pub sigset: SignatorySet,
}

impl Checkpoint {
    /// Creates a new checkpoint with the given signatory set.
    ///
    /// The checkpoint will be initialized with a single empty checkpoint
    /// transaction, a single empty intermediate emergency disbursal
    /// transaction, and an empty batch of final emergency disbursal
    /// transactions.
    pub fn new(sigset: SignatorySet) -> ContractResult<Self> {
        let mut checkpoint = Checkpoint {
            status: CheckpointStatus::default(),
            fee_rate: DEFAULT_FEE_RATE,
            signed_at_btc_height: None,
            deposits_enabled: true,
            sigset,
            fees_collected: 0,
            pending: vec![],
            batches: vec![],
        };

        let disbursal_batch = Batch::default();
        checkpoint.batches.push(disbursal_batch);

        let mut intermediate_tx_batch = Batch::default();
        intermediate_tx_batch.push(BitcoinTx::default());
        checkpoint.batches.push(intermediate_tx_batch);

        let checkpoint_tx = BitcoinTx::default();
        let mut checkpoint_batch = Batch::default();
        checkpoint_batch.push(checkpoint_tx);
        checkpoint.batches.push(checkpoint_batch);

        Ok(checkpoint)
    }

    /// Changes the status of the checkpoint to `Complete`.
    pub fn advance(&mut self) {
        self.status = CheckpointStatus::Complete;
    }

    /// Processes a batch of signatures from a signatory, applying them to the
    /// inputs of transaction batches which are ready to be signed.
    ///
    /// Transaction batches are ready to be signed if they are either already
    /// signed (all inputs of all transactions in the batch are above the
    /// signing threshold), in which case any newly-submitted signatures will
    /// "over-sign" the inputs, or if the batch is the first non-signed batch
    /// (the "active" batch). This prevents signatories from submitting
    /// signatures to a batch beyond the active batch, so that batches are
    /// always finished signing serially, in order.
    ///
    /// A signatory must submit all signatures for all inputs in which they are
    /// present in the signatory set, for all transactions of all batches ready
    /// to be signed. If the signatory provides more or less signatures than
    /// expected, `sign()` will return an error.
    fn sign(&mut self, xpub: &Xpub, sigs: Vec<Signature>, btc_height: u32) -> ContractResult<()> {
        let secp = bitcoin::secp256k1::Secp256k1::verification_only();

        let cp_was_signed = self.signed();
        let mut sig_index = 0;

        // Iterate over all batches in the checkpoint, breaking once iterating
        // to a batch which is not ready to be signed.
        for batch in &mut self.batches {
            let batch_was_signed = batch.signed();

            // Iterate over all transactions in the batch.
            for tx in &mut batch.batch {
                let tx_was_signed = tx.signed();

                // Iterate over all inputs in the transaction.
                for k in 0..tx.input.len() {
                    let input = tx.input.get_mut(k).unwrap();
                    let pubkey = derive_pubkey(&secp, xpub, input.sigset_index)?;

                    // Skip input if either the signatory is not part of this
                    // input's signatory set, or the signatory has already
                    // submitted a signature for this input.
                    if !input.signatures.needs_sig(pubkey.into()) {
                        continue;
                    }

                    // Error if there are no remaining supplied signatures - the
                    // signatory supplied less signatures than we require from
                    // them.
                    if sig_index >= sigs.len() {
                        return Err(ContractError::Checkpoint(
                            "Not enough signatures supplied".into(),
                        ));
                    }
                    let sig = &sigs[sig_index];
                    sig_index += 1;

                    // Apply the signature.
                    let input_was_signed = input.signatures.signed();
                    input.signatures.sign(pubkey.into(), sig)?;

                    // If this signature made the input fully signed, increase
                    // the counter of fully-signed inputs in the containing
                    // transaction.
                    if !input_was_signed && input.signatures.signed() {
                        tx.signed_inputs += 1;
                    }
                }

                // If these signatures made the transaction fully signed,
                // increase the counter of fully-signed transactions in the
                // containing batch.
                if !tx_was_signed && tx.signed() {
                    batch.signed_txs += 1;
                }
            }

            // If this was the last batch ready to be signed, stop here.
            if !batch_was_signed {
                break;
            }
        }

        // Error if there are remaining supplied signatures - the signatory
        // supplied more signatures than we require from them.
        if sig_index != sigs.len() {
            return Err(ContractError::Checkpoint(
                "Excess signatures supplied".into(),
            ));
        }

        // If these signatures made the checkpoint fully signed, record the
        // height at which it was signed.
        if self.signed() && !cp_was_signed {
            self.signed_at_btc_height = Some(btc_height);
        }

        Ok(())
    }

    /// Gets the checkpoint transaction as a `bitcoin::Transaction`.    
    pub fn checkpoint_tx(&self) -> ContractResult<Adapter<bitcoin::Transaction>> {
        Ok(Adapter::new(
            self.batches
                .get(BatchType::Checkpoint as usize)
                .unwrap()
                .last()
                .unwrap()
                .to_bitcoin_tx()?,
        ))
    }

    /// Gets the output containing the reserve funds for the checkpoint, the
    /// "reserve output". This output is owned by the latest signatory set, and
    /// is spent by the suceeding checkpoint transaction.
    ///
    /// This output is not created until the checkpoint advances to `Signing`
    /// status.
    pub fn reserve_output(&self) -> ContractResult<Option<TxOut>> {
        // TODO: should return None for Building checkpoints? otherwise this
        // might return a withdrawal
        let checkpoint_tx = self.checkpoint_tx()?;
        if let Some(output) = checkpoint_tx.output.first() {
            Ok(Some(output.clone()))
        } else {
            Ok(None)
        }
    }

    /// Returns a list of all inputs in the checkpoint which the signatory with
    /// the given extended public key should sign.
    ///
    /// The return value is a list of tuples, each containing `(sighash,
    /// sigset_index)` - the sighash to be signed and the index of the signatory
    /// set associated with the input.    
    pub fn to_sign(&self, xpub: &Xpub) -> ContractResult<Vec<([u8; 32], u32)>> {
        // TODO: thread local secpk256k1 context
        let secp = bitcoin::secp256k1::Secp256k1::verification_only();

        let mut msgs = vec![];

        for batch in &self.batches {
            for tx in &batch.batch {
                for input in &tx.input {
                    let pubkey = derive_pubkey(&secp, xpub, input.sigset_index)?;
                    if input.signatures.needs_sig(pubkey.into()) {
                        msgs.push((input.signatures.message(), input.sigset_index));
                    }
                }
            }
            if !batch.signed() {
                break;
            }
        }

        Ok(msgs)
    }

    /// Returns the number of fully-signed batches in the checkpoint.
    fn signed_batches(&self) -> u64 {
        let mut signed_batches = 0;
        for batch in &self.batches {
            if batch.signed() {
                signed_batches += 1;
            } else {
                break;
            }
        }

        signed_batches
    }

    /// Returns the current batch being signed, or `None` if all batches are
    /// signed.
    pub fn current_batch(&self) -> Option<Batch> {
        if self.signed() {
            return None;
        }
        let pos = self.signed_batches();
        self.batches.get(pos as usize).cloned()
    }

    /// Returns the timestamp at which the checkpoint was created (when it was
    /// first constructed in the `Building` status).
    pub fn create_time(&self) -> u64 {
        self.sigset.create_time()
    }

    /// Returns `true` if all batches in the checkpoint are fully signed,
    /// otherwise returns `false`.
    pub fn signed(&self) -> bool {
        self.signed_batches() == self.batches.len() as u64
    }

    /// The emergency disbursal transactions for checkpoint.
    ///
    /// The first element of the returned vector is the intermediate
    /// transaction, and the remaining elements are the final transactions.
    pub fn emergency_disbursal_txs(&self) -> ContractResult<Vec<Adapter<bitcoin::Transaction>>> {
        let mut txs = vec![];

        let intermediate_tx_batch = self
            .batches
            .get(BatchType::IntermediateTx as usize)
            .unwrap();
        let Some(intermediate_tx) = intermediate_tx_batch.get(0) else {
            return Ok(txs);
        };
        txs.push(Adapter::new(intermediate_tx.to_bitcoin_tx()?));

        let disbursal_batch = self.batches.get(BatchType::Disbursal as usize).unwrap();
        for tx in disbursal_batch.iter() {
            txs.push(Adapter::new(tx.to_bitcoin_tx()?));
        }

        Ok(txs)
    }

    pub fn checkpoint_tx_miner_fees(&self) -> ContractResult<u64> {
        let mut fees = 0;

        let batch = self.batches.get(BatchType::Checkpoint as usize).unwrap();
        let tx = batch.get(0).unwrap();

        for input in &tx.input {
            fees += input.amount;
        }

        for output in &tx.output {
            fees -= output.value;
        }

        Ok(fees)
    }

    pub fn base_fee(
        &self,
        config: &CheckpointConfig,
        timestamping_commitment: &[u8],
    ) -> ContractResult<u64> {
        let est_vsize = self.est_vsize(config, timestamping_commitment)?;
        Ok(est_vsize * self.fee_rate)
    }

    fn est_vsize(
        &self,
        config: &CheckpointConfig,
        timestamping_commitment: &[u8],
    ) -> ContractResult<u64> {
        let batch = self.batches.get(BatchType::Checkpoint as usize).unwrap();
        let cp = batch.get(0).unwrap();
        let mut tx = cp.to_bitcoin_tx()?;

        tx.output = self
            .additional_outputs(config, timestamping_commitment)?
            .into_iter()
            .chain(tx.output)
            .take(config.max_outputs as usize)
            .collect();
        tx.input.truncate(config.max_inputs as usize);

        let vsize = tx.vsize() as u64
            + cp.input
                .iter()
                .take(config.max_inputs as usize)
                .try_fold(0, |sum, input| {
                    Ok::<_, ContractError>(sum + input.est_witness_vsize)
                })?;

        Ok(vsize)
    }

    // This function will return total input amount and output amount in checkpoint transaction
    pub fn calc_total_input_and_output(
        &self,
        config: &CheckpointConfig,
    ) -> ContractResult<(u64, u64)> {
        let mut in_amount = 0;
        let checkpoint_batch =
            self.batches
                .get(BatchType::Checkpoint as usize)
                .ok_or(ContractError::Checkpoint(
                    "Cannot get batch checkpoint".into(),
                ))?;
        let checkpoint_tx = checkpoint_batch
            .get(0)
            .ok_or(ContractError::Checkpoint("Cannot get checkpoint tx".into()))?;
        for i in 0..config.max_inputs.min(checkpoint_tx.input.len() as u64) {
            let input = checkpoint_tx
                .input
                .get(i as usize)
                .ok_or(ContractError::Checkpoint(
                    "Cannot get checkpoint tx input".into(),
                ))?;
            in_amount += input.amount;
        }
        let mut out_amount = 0;
        for i in 0..config.max_outputs.min(checkpoint_tx.output.len() as u64) {
            let output = checkpoint_tx
                .output
                .get(i as usize)
                .ok_or(ContractError::Checkpoint(
                    "Cannot get checkpoint tx output".into(),
                ))?;
            out_amount += output.value;
        }
        Ok((in_amount, out_amount))
    }

    fn additional_outputs(
        &self,
        config: &CheckpointConfig,
        timestamping_commitment: &[u8],
    ) -> ContractResult<Vec<bitcoin::TxOut>> {
        // The reserve output is the first output of the checkpoint tx, and
        // contains all funds held in reserve by the network.
        let reserve_out = bitcoin::TxOut {
            value: 0, // will be updated after counting ins/outs and fees
            script_pubkey: self.sigset.output_script(&[0u8], config.sigset_threshold)?,
        };

        // The timestamping commitment output is the second output of the
        // checkpoint tx, and contains a commitment to some given data, which
        // will be included on the Bitcoin blockchain as `OP_RETURN` data, now
        // timestamped by Bitcoin's proof-of-work security.
        let timestamping_commitment_out = bitcoin::TxOut {
            value: 0,
            script_pubkey: bitcoin::Script::new_op_return(timestamping_commitment),
        };

        Ok(vec![reserve_out, timestamping_commitment_out])
    }
}

/// `CheckpointQueue` is the main collection for the checkpointing process,
/// containing a sequential chain of checkpoints.
///
/// Once the network has processed its first deposit, the checkpoint queue will
/// always contain at least one checkpoint, in the `Building` state, at the
/// highest index in the queue.
///
/// The queue will only contain at most one checkpoint in the `Signing` state,
/// at the second-highest index in the queue if it exists. When this checkpoint
/// is stil being signed, progress will block and no new checkpoints will be
/// created since the checkpoints are in a sequential chain.
///
/// The queue may contain any number of checkpoints in the `Complete` state,
/// which are the checkpoints which have been fully signed and are ready to be
/// broadcast to the Bitcoin network. The queue also maintains a counter
/// (`confirmed_index`) to track which of these completed checkpoints have been
/// confirmed in a Bitcoin block.
#[cw_serde]
#[derive(Default)]
pub struct CheckpointQueue {
    /// The index of the checkpoint currently being built.
    pub index: u32,

    /// The index of the last checkpoint which has been confirmed in a Bitcoin
    /// block. Since checkpoints are a sequential cahin, each spending an output
    /// from the previous, all checkpoints with an index lower than this must
    /// have also been confirmed.
    pub confirmed_index: Option<u32>,

    // the first confirmed checkpoint that we have not handled its pending deposit
    pub first_unhandled_confirmed_cp_index: u32,

    /// Configuration parameters used in processing checkpoints.
    pub config: CheckpointConfig,

    /// The checkpoints in the queue, in order from oldest to newest. The last
    /// checkpoint is the checkpoint currently being built, and has the index
    /// contained in the `index` field.
    pub queue_id: String,
}

impl CheckpointQueue {
    pub fn new(store: &mut dyn Storage) -> ContractResult<Self> {
        let queue_id = format!(
            "{}{}s",
            CHECKPOINT_QUEUE_ID_PREFIX,
            next_checkpoint_queue_id(store)?
        );
        Ok(Self {
            queue_id,
            ..Default::default()
        })
    }
}

/// A wrapper around  an immutable reference to a `Checkpoint` which adds type
/// information guaranteeing that the checkpoint is in the `Complete` state.
#[derive(Deref)]
pub struct CompletedCheckpoint(Box<Checkpoint>);

/// A wrapper around a mutable reference to a `Checkpoint` which adds type
/// information guaranteeing that the checkpoint is in the `Complete` state.
#[derive(Deref, DerefMut)]
pub struct SigningCheckpoint(Box<Checkpoint>);

impl SigningCheckpoint {
    /// Adds a batch of signatures to the checkpoint for the signatory with the
    /// given extended public key (`xpub`).
    ///
    /// The signatures must be provided in the same order as the inputs in the
    /// checkpoint transaction, and must be provided for all inputs in which the
    /// signatory is present in the signatory set.
    pub fn sign(
        &mut self,
        xpub: Xpub,
        sigs: Vec<Signature>,
        btc_height: u32,
    ) -> ContractResult<()> {
        self.0.sign(&xpub, sigs, btc_height)?;
        Ok(())
    }
}

/// A wrapper around a mutable reference to a `Checkpoint` which adds type
/// information guaranteeing that the checkpoint is in the `Building` state.
#[derive(Deref, DerefMut)]
pub struct BuildingCheckpoint(Box<Checkpoint>);

/// The data returned by the `advance()` method of `BuildingCheckpointMut`.
type BuildingAdvanceRes = (
    bitcoin::OutPoint, // reserve outpoint
    u64,               // reserve size (sats)
    u64,               // fees paid (sats)
    Vec<Input>,        // excess inputs
    Vec<Output>,       // excess outputs
);

impl BuildingCheckpoint {
    /// Adds an output to the intermediate emergency disbursal transaction of
    /// the checkpoint, to be spent by the given final emergency disbursal
    /// transaction. The corresponding input is also added to the final
    /// emergency disbursal transaction.
    fn link_intermediate_tx(
        &mut self,
        tx: &mut BitcoinTx,
        threshold: (u64, u64),
    ) -> ContractResult<()> {
        let sigset = self.sigset.clone();
        let output_script = sigset.output_script(&[0u8], threshold)?;
        let tx_value = tx.value()?;

        let intermediate_tx_batch = self
            .batches
            .get_mut(BatchType::IntermediateTx as usize)
            .unwrap();
        let intermediate_tx = intermediate_tx_batch.get_mut(0).unwrap();
        let num_outputs = u32::try_from(intermediate_tx.output.len())?;

        let prevout = bitcoin::OutPoint::new(intermediate_tx.txid()?, num_outputs);
        let final_tx_input = Input::new(prevout, &sigset, &[0u8], tx_value, threshold)?;

        let intermediate_tx_output = bitcoin::TxOut {
            value: tx_value,
            script_pubkey: output_script,
        };

        intermediate_tx.output.push(intermediate_tx_output.into());

        tx.input.push(final_tx_input);

        Ok(())
    }

    /// Deducts satoshis from the outputs of all emergency disbursal
    /// transactions (the intermediate transaction and all final transactions)
    /// to make them pay the miner fee at the given fee rate.
    ///
    /// Any outputs which are too small to pay their share of the required fees
    /// will be removed.
    ///
    /// It is possible for this process to remove outputs from the intermediate
    /// transaction, leaving an orphaned final transaction which spends from a
    /// non-existent output. for simplicity the unconnected final transaction is
    /// left in the state (it can be skipped by relayers when broadcasting the
    /// remaining valid emergency disbursal transactions).
    fn deduct_emergency_disbursal_fees(&mut self, fee_rate: u64) -> ContractResult<()> {
        // TODO: Unit tests
        // Deduct fees from intermediate emergency disbursal transaction.
        // Let-binds the amount deducted so we can ensure to deduct the same
        // amount from the final emergency disbursal transactions since the
        // outputs they spend are now worth less than before.
        let intermediate_tx_fee = {
            let intermediate_tx_batch = self
                .batches
                .get_mut(BatchType::IntermediateTx as usize)
                .unwrap();
            let intermediate_tx = intermediate_tx_batch.get_mut(0).unwrap();
            let fee = intermediate_tx.vsize()? * fee_rate;
            intermediate_tx.deduct_fee(fee)?;
            fee
        };

        let intermediate_tx_batch = self
            .batches
            .get(BatchType::IntermediateTx as usize)
            .unwrap();
        let intermediate_tx = intermediate_tx_batch.get(0).unwrap();
        let intermediate_tx_id = intermediate_tx.txid()?;
        let intermediate_tx_len = intermediate_tx.output.len() as u64;

        if intermediate_tx_len == 0 {
            println!("Generated empty emergency disbursal");
            return Ok(());
        }

        // Collect a list of the outputs of the intermediate emergency
        // disbursal, so later on we can ensure there is a 1-to-1 mapping
        // between final transactions and intermediate outputs, matched by
        // amount.
        let mut intermediate_tx_outputs: Vec<(usize, u64)> = intermediate_tx
            .output
            .iter()
            .enumerate()
            .map(|(i, output)| (i, output.value))
            .collect();

        // Deduct fees from final emergency disbursal transactions. Only retain
        // transactions which have enough value to pay the fee.
        let disbursal_batch = self.batches.get_mut(BatchType::Disbursal as usize).unwrap();
        disbursal_batch.batch = disbursal_batch
            .batch
            .clone()
            .into_iter()
            .filter_map(|mut tx| {
                // Do not retain transactions which were never linked to the
                // intermediate tx.
                // TODO: is this even possible?
                let input = match tx.input.get_mut(0) {
                    Some(input) => input,
                    None => return None,
                };

                // Do not retain transactions which are smaller than the amount of
                // fee applied to the intermediate tx output which they spend. If
                // large enough, deduct the fee from the input to match what was
                // already deducted for the intermediate tx output.
                if input.amount < intermediate_tx_fee / intermediate_tx_len {
                    return None;
                }
                input.amount -= intermediate_tx_fee / intermediate_tx_len;

                // Find the first remaining output of the intermediate tx which
                // matches the amount being spent by this final tx's input.
                for (i, (vout, output)) in intermediate_tx_outputs.iter().enumerate() {
                    if output == &(input.amount) {
                        // Once found, link the final tx's input to the vout index
                        // of the the matching output from the intermediate tx, and
                        // remove it from the matching list.

                        input.prevout = Adapter::new(bitcoin::OutPoint {
                            txid: intermediate_tx_id,
                            vout: *vout as u32,
                        });
                        intermediate_tx_outputs.remove(i);
                        // Deduct the final tx's miner fee from its outputs,
                        // removing any outputs which are too small to pay their
                        // share of the fee.
                        let tx_size = tx.vsize().unwrap();
                        let fee = intermediate_tx_fee / intermediate_tx_len + tx_size * fee_rate;
                        tx.deduct_fee(fee).unwrap();
                        return Some(tx);
                    }
                }
                None
            })
            .collect();
        Ok(())
    }

    /// Generates the emergency disbursal transactions for the checkpoint,
    /// populating the first and second transaction batches in the checkpoint.
    ///
    /// The emergency disbursal transactions are generated from a list of
    /// outputs representing the holders of nBTC: one for every nBTC account
    /// which has an associated recovery script, one for every pending transfer
    /// in the checkpoint, and one for every output passed in by the consumer
    /// via the `external_outputs` iterator.
    #[allow(clippy::too_many_arguments)]
    fn generate_emergency_disbursal_txs(
        &mut self,
        env: Env,
        store: &mut dyn Storage,
        nbtc_accounts: &Accounts,
        reserve_outpoint: bitcoin::OutPoint,
        external_outputs: impl Iterator<Item = ContractResult<bitcoin::TxOut>>,
        fee_rate: u64,
        reserve_value: u64,
        config: &CheckpointConfig,
    ) -> ContractResult<()> {
        // TODO: Use tree structure instead of single-intermediate, many-final,
        // since the intermediate tx may grow too large
        let intermediate_tx_batch = self
            .batches
            .get_mut(BatchType::IntermediateTx as usize)
            .unwrap();
        if intermediate_tx_batch.is_empty() {
            return Ok(());
        }

        let sigset = self.sigset.clone();

        let lock_time =
            env.block.time.seconds() as u32 + config.emergency_disbursal_lock_time_interval;

        let mut outputs = Vec::new();

        // Create an output for every nBTC account with an associated
        // recovery script.
        for script in RECOVERY_SCRIPTS
            .range(store, None, None, Order::Ascending)
            .into_iter()
        {
            let (address, dest_script) = script?;
            let balance = nbtc_accounts.balance(address).unwrap();
            let tx_out = bitcoin::TxOut {
                value: (balance.amount.u128() / 1_000_000u128) as u64,
                script_pubkey: dest_script.clone().into_inner(),
            };

            outputs.push(Ok(tx_out))
        }

        // Create an output for every pending nBTC transfer in the checkpoint.
        // TODO: combine pending transfer outputs into other outputs by adding to amount
        let pending_outputs: Vec<_> = self
            .pending
            .iter()
            .filter_map(|(dest, coin)| {
                let script_pubkey = match to_output_script(store, dest) {
                    Err(err) => return Some(Err(err.into())),
                    Ok(maybe_script) => maybe_script,
                }?;
                Some(Ok::<_, ContractError>(TxOut {
                    value: (coin.amount.u128() / 1_000_000u128) as u64,
                    script_pubkey,
                }))
            })
            .collect();

        // Iterate through outputs and batch them into final txs, adding
        // outputs to the intermediate tx and linking inputs to them as we
        // go.
        let mut final_txs = vec![BitcoinTx::with_lock_time(lock_time)];
        for output in outputs
            .into_iter()
            .chain(pending_outputs.into_iter())
            .chain(external_outputs)
        {
            let output = output?;

            // Skip outputs under the configured minimum amount.
            if output.value < config.emergency_disbursal_min_tx_amt {
                continue;
            }

            // If the last final tx is too large, create a new, empty one
            // and add our output there instead.
            // TODO: don't pop and repush, just get a mutable reference
            let mut curr_tx = final_txs.pop().unwrap();
            if curr_tx.vsize()? >= config.emergency_disbursal_max_tx_size {
                self.link_intermediate_tx(&mut curr_tx, config.sigset_threshold)?;
                final_txs.push(curr_tx);
                curr_tx = BitcoinTx::with_lock_time(lock_time);
            }

            // Add output to final tx.
            curr_tx.output.push(Adapter::new(output));

            final_txs.push(curr_tx);
        }

        // We are done adding outputs, so link the last final tx to the
        // intermediate tx.
        let mut last_tx = final_txs.pop().unwrap();
        self.link_intermediate_tx(&mut last_tx, config.sigset_threshold)?;
        final_txs.push(last_tx);

        // Add the reserve output as an input to the intermediate tx, and
        // set its locktime to the desired value.
        let tx_in = Input::new(
            reserve_outpoint,
            &sigset,
            &[0u8],
            reserve_value,
            config.sigset_threshold,
        )?;
        let output_script = self.sigset.output_script(&[0u8], config.sigset_threshold)?;
        let intermediate_tx_batch = self
            .batches
            .get_mut(BatchType::IntermediateTx as usize)
            .unwrap();
        let intermediate_tx = intermediate_tx_batch.get_mut(0).unwrap();
        intermediate_tx.lock_time = lock_time;
        intermediate_tx.input.push(tx_in);

        // For any excess value not accounted for by emergency disbursal
        // outputs, add an output to the intermediate tx which pays the
        // excess back to the signatory set. The signatory set will need to
        // coordinate out-of-band to figure out how to deal with these
        // unaccounted-for funds to return them to the rightful nBTC
        // holders.
        let intermediate_tx_out_value = intermediate_tx.value()?;
        let excess_value = reserve_value - intermediate_tx_out_value;
        let excess_tx_out = bitcoin::TxOut {
            value: excess_value,
            script_pubkey: output_script,
        };
        intermediate_tx.output.push(Adapter::new(excess_tx_out));

        // Push the newly created final txs into the checkpoint batch to
        // save them in the state.
        let disbursal_batch = self.batches.get_mut(BatchType::Disbursal as usize).unwrap();
        for tx in final_txs {
            disbursal_batch.push(tx);
        }

        // Deduct Bitcoin miner fees from the intermediate tx and all final txs.
        self.deduct_emergency_disbursal_fees(fee_rate)?;

        // Populate the sighashes to be signed for each final tx's input.
        let disbursal_batch = self.batches.get_mut(BatchType::Disbursal as usize).unwrap();
        for i in 0..disbursal_batch.len() {
            let tx = disbursal_batch.get_mut(i).unwrap();
            for j in 0..tx.input.len() {
                tx.populate_input_sig_message(j)?;
            }
        }

        // Populate the sighashes to be signed for the intermediate tx's input.
        let intermediate_tx_batch = self
            .batches
            .get_mut(BatchType::IntermediateTx as usize)
            .unwrap();
        let intermediate_tx = intermediate_tx_batch.get_mut(0).unwrap();
        intermediate_tx.populate_input_sig_message(0)?;

        Ok(())
    }

    /// Advances the checkpoint to the `Signing` state.
    ///
    /// This will generate the emergency disbursal transactions representing the
    /// ownership of nBTC at this point in time. It will also prepare all inputs
    /// to be signed, across the three transaction batches.
    ///
    /// This step freezes the checkpoint, and no further changes can be made to
    /// it other than adding signatures. This means at this point all
    /// transactions contained within have a known transaction id which will not
    /// change.    
    pub fn advance(
        &mut self,
        env: Env,
        store: &mut dyn Storage,
        nbtc_accounts: &Accounts,
        external_outputs: impl Iterator<Item = ContractResult<bitcoin::TxOut>>,
        timestamping_commitment: Vec<u8>,
        cp_fees: u64,
        config: &CheckpointConfig,
    ) -> ContractResult<BuildingAdvanceRes> {
        self.0.status = CheckpointStatus::Signing;

        let outs = self.additional_outputs(config, &timestamping_commitment)?;
        let checkpoint_batch = self
            .batches
            .get_mut(BatchType::Checkpoint as usize)
            .unwrap();
        let checkpoint_tx = checkpoint_batch.get_mut(0).unwrap();
        for out in outs.iter().rev() {
            checkpoint_tx.output.insert(0, Adapter::new(out.clone()));
        }

        // Remove excess inputs and outputs from the checkpoint tx, to be pushed
        // onto the suceeding checkpoint while in its `Building` state.
        let mut excess_inputs = vec![];
        while checkpoint_tx.input.len() as u64 > config.max_inputs {
            let removed_input = checkpoint_tx.input.pop().unwrap();
            excess_inputs.push(removed_input);
        }
        let mut excess_outputs = vec![];
        while checkpoint_tx.output.len() as u64 > config.max_outputs {
            let removed_output = checkpoint_tx.output.pop().unwrap();
            excess_outputs.push(removed_output);
        }

        // Sum the total input and output amounts.
        // TODO: Input/Output sum functions
        let mut in_amount = 0;
        for i in 0..checkpoint_tx.input.len() {
            let input = checkpoint_tx.input.get(i).unwrap();
            in_amount += input.amount;
        }
        let mut out_amount = 0;
        for i in 0..checkpoint_tx.output.len() {
            let output = checkpoint_tx.output.get(i).unwrap();
            out_amount += output.value;
        }

        // Deduct the outgoing amount and calculated fee amount from the reserve
        // input amount, to set the resulting reserve output value.
        let reserve_value = in_amount.checked_sub(out_amount + cp_fees).ok_or_else(|| {
            ContractError::Checkpoint("Insufficient reserve value to cover miner fees".into())
        })?;
        let reserve_out = checkpoint_tx.output.get_mut(0).unwrap();
        reserve_out.value = reserve_value;

        // Prepare the checkpoint tx's inputs to be signed by calculating their
        // sighashes.
        let bitcoin_tx = checkpoint_tx.to_bitcoin_tx()?;
        let mut sc = bitcoin::util::sighash::SighashCache::new(&bitcoin_tx);
        for i in 0..checkpoint_tx.input.len() {
            let input = checkpoint_tx.input.get_mut(i).unwrap();
            let sighash = sc.segwit_signature_hash(
                i as usize,
                &input.redeem_script,
                input.amount,
                EcdsaSighashType::All,
            )?;
            input.signatures.set_message(sighash.into_inner());
        }

        // Generate the emergency disbursal transactions, spending from the
        // reserve output.
        let reserve_outpoint = bitcoin::OutPoint {
            txid: checkpoint_tx.txid()?,
            vout: 0,
        };
        self.generate_emergency_disbursal_txs(
            env,
            store,
            nbtc_accounts,
            reserve_outpoint,
            external_outputs,
            self.fee_rate,
            reserve_value,
            config,
        )?;

        Ok((
            reserve_outpoint,
            reserve_value,
            cp_fees,
            excess_inputs,
            excess_outputs,
        ))
    }

    /// Insert a transfer to the pending transfer queue.
    ///
    /// Transfers will be processed once the containing checkpoint is finished
    /// being signed, but will be represented in this checkpoint's emergency
    /// disbursal before they are processed.
    pub fn insert_pending(&mut self, dest: Dest, coin: Coin) -> ContractResult<()> {
        let dest_key = dest.to_receiver_addr();
        match self.pending.iter_mut().find(|item| item.0 == dest_key) {
            Some((_, existed_coin)) => {
                existed_coin.amount += coin.amount;
            }
            None => self.pending.push((dest_key, coin)),
        };

        Ok(())
    }
}

impl CheckpointQueue {
    pub fn queue(&self) -> DequeExtension<Checkpoint> {
        DequeExtension::new(&self.queue_id)
    }

    /// Set the queue's configuration parameters.
    pub fn configure(&mut self, config: CheckpointConfig) {
        self.config = config;
    }

    /// The queue's current configuration parameters.
    pub fn config(&self) -> CheckpointConfig {
        self.config.clone()
    }

    /// Removes all checkpoints from the queue and resets the index to zero.
    pub fn reset(&mut self, store: &mut dyn Storage) -> ContractResult<()> {
        self.index = 0;
        let checkpoints = self.queue();
        while !checkpoints.is_empty(store)? {
            checkpoints.pop_back(store)?;
        }

        Ok(())
    }

    /// Gets a reference to the checkpoint at the given index.
    ///
    /// If the index is out of bounds or was pruned, an error is returned.
    pub fn get(&self, store: &dyn Storage, index: u32) -> ContractResult<Checkpoint> {
        let checkpoints = self.queue();
        let queue_len = checkpoints.len(store)?;
        let index = self.get_deque_index(index, queue_len)?;
        let checkpoint = checkpoints.get(store, index)?.unwrap();
        Ok(checkpoint)
    }

    pub fn set(
        &self,
        store: &mut dyn Storage,
        index: u32,
        checkpoint: &Checkpoint,
    ) -> ContractResult<()> {
        let checkpoints = self.queue();
        let queue_len = checkpoints.len(store)?;
        let index = self.get_deque_index(index, queue_len)?;
        checkpoints.set(store, index, checkpoint)?;
        Ok(())
    }

    /// Calculates the index within the deque based on the given checkpoint
    /// index.
    ///
    /// This is necessary because the values can differ for queues which have
    /// been pruned. For example, a queue may contain 5 checkpoints,
    /// representing indexes 30 to 34. Checkpoint index 30 is at deque index 0,
    /// checkpoint 34 is at deque index 4, and checkpoint index 29 is now
    /// out-of-bounds.
    fn get_deque_index(&self, index: u32, queue_len: u32) -> ContractResult<u32> {
        let start = self.index + 1 - queue_len;
        if index > self.index || index < start {
            Err(ContractError::App("Index out of bounds".into()))
        } else {
            Ok(index - start)
        }
    }

    /// The number of checkpoints in the queue.
    ///
    /// This will likely be different from `index` since checkpoints can be
    /// pruned. After receiving the first deposit, the network will always have
    /// at least one checkpoint in the queue.
    // TODO: remove this attribute, not sure why clippy is complaining when
    // is_empty is defined
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self, store: &dyn Storage) -> ContractResult<u32> {
        let checkpoints = self.queue();
        let queue_len = checkpoints.len(store)?;
        Ok(queue_len)
    }

    /// Returns `true` if there are no checkpoints in the queue.
    ///
    /// This will only be `true` before the first deposit has been processed.
    pub fn is_empty(&self, store: &dyn Storage) -> ContractResult<bool> {
        Ok(self.len(store)? == 0)
    }

    /// The index of the last checkpoint in the queue (aka the `Building`
    /// checkpoint).
    pub fn index(&self) -> u32 {
        self.index
    }

    /// All checkpoints in the queue, in order from oldest to newest.
    ///
    /// The return value is a vector of tuples, where the first element is the
    /// checkpoint's index, and the second element is a reference to the
    /// checkpoint.
    pub fn all(&self, store: &dyn Storage) -> ContractResult<Vec<(u32, Checkpoint)>> {
        // TODO: return iterator
        // TODO: use DequeExtension iterator
        let checkpoints = self.queue();
        let queue_len = checkpoints.len(store)?;
        let mut out = Vec::with_capacity(queue_len as usize);

        for i in 0..queue_len {
            let checkpoint = checkpoints.get(store, i)?.unwrap();
            out.push(((self.index + 1 - (queue_len - i as u32)), checkpoint));
        }

        Ok(out)
    }

    /// All checkpoints in the queue which are in the `Complete` state, in order
    /// from oldest to newest.
    pub fn completed(
        &self,
        store: &dyn Storage,
        limit: u32,
    ) -> ContractResult<Vec<CompletedCheckpoint>> {
        // TODO: return iterator
        // TODO: use DequeExtension iterator

        let mut out = vec![];

        let length = self.len(store)?;
        if length == 0 {
            return Ok(out);
        }

        let skip = if self.signing(store)?.is_some() { 2 } else { 1 };
        let end = self.index.saturating_sub(skip - 1);

        let start = end - limit.min(length - skip);

        for i in start..end {
            let checkpoint = self.get(store, i)?;
            out.push(CompletedCheckpoint(Box::new(checkpoint)));
        }

        Ok(out)
    }

    /// The index of the last completed checkpoint.
    pub fn last_completed_index(&self, store: &dyn Storage) -> ContractResult<u32> {
        if self.signing(store)?.is_some() {
            self.index.checked_sub(2)
        } else {
            self.index.checked_sub(1)
        }
        .ok_or_else(|| ContractError::App("No completed checkpoints yet".to_string()))
    }

    pub fn first_index(&self, store: &dyn Storage) -> ContractResult<u32> {
        Ok(self.index + 1 - self.len(store)?)
    }

    /// A reference to the last completed checkpoint.
    pub fn last_completed(&self, store: &dyn Storage) -> ContractResult<Checkpoint> {
        let index = self.last_completed_index(store)?;
        self.get(store, index)
    }

    /// The last completed checkpoint, converted to a Bitcoin transaction.
    pub fn last_completed_tx(
        &self,
        store: &dyn Storage,
    ) -> ContractResult<Adapter<bitcoin::Transaction>> {
        self.last_completed(store)?.checkpoint_tx()
    }

    /// All completed checkpoints, converted to Bitcoin transactions.
    pub fn completed_txs(
        &self,
        store: &dyn Storage,
        limit: u32,
    ) -> ContractResult<Vec<Adapter<bitcoin::Transaction>>> {
        self.completed(store, limit)?
            .into_iter()
            .map(|c| c.checkpoint_tx())
            .collect()
    }

    /// The emergency disbursal transactions for the last completed checkpoint.
    ///
    /// The first element of the returned vector is the intermediate
    /// transaction, and the remaining elements are the final transactions.
    pub fn emergency_disbursal_txs(
        &self,
        store: &dyn Storage,
    ) -> ContractResult<Vec<Adapter<bitcoin::Transaction>>> {
        if let Some(completed) = self.completed(store, 1)?.last() {
            completed.emergency_disbursal_txs()
        } else {
            Ok(vec![])
        }
    }

    /// The last complete builiding checkpoint transaction, which have the type BatchType::Checkpoint
    /// Here we have only one element in the vector, and I use vector because I don't want to throw
    /// any error if vec! is empty
    pub fn last_completed_checkpoint_tx(
        &self,
        store: &dyn Storage,
    ) -> ContractResult<Vec<Adapter<bitcoin::Transaction>>> {
        let mut txs = vec![];
        if let Some(completed) = self.completed(store, 1)?.last() {
            txs.push(completed.checkpoint_tx()?);
            Ok(txs)
        } else {
            Ok(vec![])
        }
    }

    /// A reference to the checkpoint in the `Signing` state, if there is one.
    pub fn signing(&self, store: &dyn Storage) -> ContractResult<Option<SigningCheckpoint>> {
        if self.len(store)? < 2 {
            return Ok(None);
        }

        let second = self.get(store, self.index - 1)?;
        if !matches!(second.status, CheckpointStatus::Signing) {
            return Ok(None);
        }

        Ok(Some(SigningCheckpoint(Box::new(second))))
    }

    /// A reference to the checkpoint in the `Building` state.
    ///
    /// This is the checkpoint which is currently being built, and is not yet
    /// being signed. Other than at the start of the network, before the first
    /// deposit has been received, there will always be a checkpoint in this
    /// state.
    pub fn building(&self, store: &dyn Storage) -> ContractResult<BuildingCheckpoint> {
        let last = self.get(store, self.index)?;
        Ok(BuildingCheckpoint(Box::new(last)))
    }

    /// Advances the checkpoint queue state machine.
    ///
    /// This method is called once per sidechain block, and will handle adding
    /// new checkpoints to the queue, advancing the `Building` checkpoint to
    /// `Signing`, and adjusting the checkpoint fee rates.
    ///
    /// If the `Building` checkpoint was advanced to `Signing` and a new
    /// `Building` checkpoint was created, this method will return `Ok(true)`.
    /// Otherwise, it will return `Ok(false)`.
    ///
    /// **Parameters:**
    ///
    /// - `sig_keys`: a map of consensus keys to their corresponding xpubs. This
    /// is used to determine which keys should be used in the signatory set,
    /// getting the set participation from the current validator set.
    /// - `nbtc_accounts`: a map of nBTC accounts to their corresponding
    /// balances. This is used along with to create outputs for the emergency
    /// disbursal transactions by getting the recovery script for each account
    /// from the `recovery_scripts` parameter.
    /// - `recovery_scripts`: a map of nBTC account addresses to their
    /// corresponding recovery scripts (account holders' desired destinations
    /// for the emergency disbursal).
    /// - `external_outputs`: an iterator of Bitcoin transaction outputs which
    /// should be included in the emergency disbursal transactions. This allows
    /// higher level modules the ability to create outputs for their own
    /// purposes.
    /// - `btc_height`: the current Bitcoin block height.
    /// - `should_allow_deposits`: whether or not deposits should be allowed in
    ///   any newly-created checkpoints.
    /// - `timestamping_commitment`: the data to be timestamped by the
    ///  checkpoint's timestamping commitment output (included as `OP_RETURN`
    ///  data in the checkpoint transaction to timestamp on the Bitcoin
    ///  blockchain for proof-of-work security).    
    #[allow(clippy::too_many_arguments)]
    pub fn maybe_step(
        &mut self,
        env: Env,
        store: &mut dyn Storage,
        nbtc_accounts: &Accounts,
        external_outputs: impl Iterator<Item = ContractResult<bitcoin::TxOut>>,
        btc_height: u32,
        should_allow_deposits: bool,
        timestamping_commitment: Vec<u8>,
        fee_pool: &mut i64,
        parent_config: &BitcoinConfig,
    ) -> ContractResult<bool> {
        if !self.should_push(env.clone(), store, &timestamping_commitment, btc_height)? {
            return Ok(false);
        }

        if self
            .maybe_push(env.clone(), store, should_allow_deposits)?
            .is_none()
        {
            return Ok(false);
        }

        self.prune(store)?;

        if self.index > 0 {
            let prev_index = self.index - 1;
            let cp_fees = self.calc_fee_checkpoint(store, prev_index, &timestamping_commitment)?;

            let config = self.config();
            let prev = self.get(store, prev_index)?;
            let sigset = prev.sigset.clone();
            let prev_fee_rate = prev.fee_rate;
            let mut building_checkpoint = BuildingCheckpoint(Box::new(prev));
            let (reserve_outpoint, reserve_value, fees_paid, excess_inputs, excess_outputs) =
                building_checkpoint.advance(
                    env,
                    store,
                    nbtc_accounts,
                    external_outputs,
                    timestamping_commitment,
                    cp_fees,
                    &config,
                )?;
            // update checkpoint
            self.set(store, prev_index, &**building_checkpoint)?;

            *fee_pool -= (fees_paid * parent_config.units_per_sat) as i64;

            // Adjust the fee rate for the next checkpoint based on whether past
            // checkpoints have been confirmed in greater or less than the
            // target number of Bitcoin blocks.
            let fee_rate = if let Some(first_unconf_index) = self.first_unconfirmed_index(store)? {
                // There are unconfirmed checkpoints.

                let first_unconf = self.get(store, first_unconf_index)?;
                let btc_blocks_since_first =
                    btc_height - first_unconf.signed_at_btc_height.unwrap_or(0);
                let miners_excluded_cps =
                    btc_blocks_since_first >= config.target_checkpoint_inclusion;

                let last_unconf_index = self.last_completed_index(store)?;
                let last_unconf = self.get(store, last_unconf_index)?;
                let btc_blocks_since_last =
                    btc_height - last_unconf.signed_at_btc_height.unwrap_or(0);
                let block_was_mined = btc_blocks_since_last > 0;

                if miners_excluded_cps && block_was_mined {
                    // Blocks were mined since a signed checkpoint, but it was
                    // not included.
                    adjust_fee_rate(prev_fee_rate, true, &config)
                } else {
                    prev_fee_rate
                }
            } else {
                let has_completed = self.last_completed_index(store).is_ok();
                if has_completed {
                    // No unconfirmed checkpoints.
                    adjust_fee_rate(prev_fee_rate, false, &config)
                } else {
                    // This case only happens at start of chain - having no
                    // unconfs doesn't mean anything.
                    prev_fee_rate
                }
            };

            let mut building = self.building(store)?;
            building.fee_rate = fee_rate;
            let building_checkpoint_batch = building
                .batches
                .get_mut(BatchType::Checkpoint as usize)
                .unwrap();
            let checkpoint_tx = building_checkpoint_batch.get_mut(0).unwrap();

            // The new checkpoint tx's first input is the reserve output from
            // the previous checkpoint.
            let input = Input::new(
                reserve_outpoint,
                &sigset,
                &[0u8], // TODO: double-check safety
                reserve_value,
                config.sigset_threshold,
            )?;
            checkpoint_tx.input.push(input);

            // Add any excess inputs and outputs from the previous checkpoint to
            // the new checkpoint.
            for input in excess_inputs {
                let shares = input.signatures.shares();
                let mut data = input.clone();
                data.signatures = ThresholdSig::from_shares(shares);
                checkpoint_tx.input.push(data);
            }
            for output in excess_outputs {
                checkpoint_tx.output.push(output);
            }

            self.set(store, self.index, &**building)?;
        }

        Ok(true)
    }

    /// Prunes old checkpoints from the queue.
    pub fn prune(&mut self, store: &mut dyn Storage) -> ContractResult<()> {
        let latest = self.building(store)?.create_time();
        let checkpoints = self.queue();
        let mut queue_len = checkpoints.len(store)?;
        while let Some(oldest) = checkpoints.front(store)? {
            // TODO: move to min_checkpoints field in config
            if queue_len <= 10 {
                break;
            }

            if latest - oldest.create_time() <= self.config.max_age {
                break;
            }

            checkpoints.pop_front(store)?;
            queue_len -= 1;
        }

        Ok(())
    }

    pub fn should_push(
        &mut self,
        env: Env,
        store: &dyn Storage,
        timestamping_commitment: &[u8],
        btc_height: u32,
    ) -> ContractResult<bool> {
        // Do not push if there is a checkpoint in the `Signing` state. There
        // should only ever be at most one checkpoint in this state.
        if self.signing(store)?.is_some() {
            return Ok(false);
        }
        let checkpoints = self.queue();
        if !checkpoints.is_empty(store)? {
            let now = env.block.time.seconds();
            let elapsed = now - self.building(store)?.create_time();

            // Do not push if the minimum checkpoint interval has not elapsed
            // since creating the current `Building` checkpoint.
            if elapsed < self.config.min_checkpoint_interval {
                return Ok(false);
            }

            // Do not push if Bitcoin headers are being backfilled (e.g. the
            // current latest height is less than the height at which the last
            // confirmed checkpoint was signed).
            if let Ok(last_completed_index) = self.last_completed_index(store) {
                let last_completed = self.get(store, last_completed_index)?;
                let last_signed_height = last_completed.signed_at_btc_height.unwrap_or(0);
                if btc_height < last_signed_height {
                    return Ok(false);
                }
            }
            let cp_miner_fees =
                self.calc_fee_checkpoint(store, self.index, timestamping_commitment)?;
            let building = self.building(store)?;

            // Don't push if there are no pending deposits, withdrawals, or
            // transfers, or if not enough has been collected to pay for the
            // miner fee, unless the maximum checkpoint interval has elapsed
            // since creating the current `Building` checkpoint.
            if elapsed < self.config.max_checkpoint_interval || self.index == 0 {
                let checkpoint_tx = building.checkpoint_tx()?;
                let has_pending_deposit = if self.index == 0 {
                    !checkpoint_tx.input.is_empty()
                } else {
                    checkpoint_tx.input.len() > 1
                };

                let has_pending_withdrawal = !checkpoint_tx.output.is_empty();
                let has_pending_transfers = building.pending.iter().next().is_some();

                if !has_pending_deposit && !has_pending_withdrawal && !has_pending_transfers {
                    return Ok(false);
                }

                if building.fees_collected < cp_miner_fees {
                    println!(
                        "Not enough collected to pay miner fee: {} < {}",
                        building.fees_collected, cp_miner_fees,
                    );
                    return Ok(false);
                }
            }

            // Do not push if the reserve value is not enough to spend the output & miner fees
            let (input_amount, output_amount) =
                building.calc_total_input_and_output(&self.config)?;
            if input_amount < output_amount + cp_miner_fees {
                println!(
                    "Total reserve value is not enough to spend the output + miner fee: {} < {}. Output amount: {}; cp_miner_fees: {}",
                    input_amount,
                    output_amount + cp_miner_fees,
                    output_amount,
                    cp_miner_fees
                );
                return Ok(false);
            }
        }

        // Do not push if there are too many unconfirmed checkpoints.
        //
        // If there is a long chain of unconfirmed checkpoints, there is possibly an
        // issue causing the transactions to not be included on Bitcoin (e.g. an
        // invalid transaction was created, the fee rate is too low even after
        // adjustments, Bitcoin miners are censoring the transactions, etc.), in
        // which case the network should evaluate and fix the issue before creating
        // more checkpoints.
        //
        // This will also stop the fee rate from being adjusted too high if the
        // issue is simply with relayers failing to report the confirmation of the
        // checkpoint transactions.
        let unconfs = self.num_unconfirmed(store)?;
        if unconfs >= self.config.max_unconfirmed_checkpoints {
            return Ok(false);
        }

        // Increment the index. For the first checkpoint, leave the index at
        // zero.
        let mut index = self.index;
        if !checkpoints.is_empty(store)? {
            index += 1;
        }

        // Build the signatory set for the new checkpoint based on the current
        // validator set.
        let sigset = SignatorySet::from_validator_ctx(store, env, index)?;
        // Do not push if there are no validators in the signatory set.
        if sigset.possible_vp() == 0 {
            return Ok(false);
        }

        // Do not push if the signatory set does not have a quorum.
        if !sigset.has_quorum() {
            return Ok(false);
        }

        // Otherwise, push a new checkpoint.
        Ok(true)
    }

    pub fn calc_fee_checkpoint(
        &self,
        store: &dyn Storage,
        cp_index: u32,
        timestamping_commitment: &[u8],
    ) -> ContractResult<u64> {
        let cp = self.get(store, cp_index)?;
        let additional_fees = self.fee_adjustment(store, cp.fee_rate, &self.config)?;
        let base_fee = cp.base_fee(&self.config, timestamping_commitment)?;
        let total_fee = base_fee + additional_fees;

        Ok(total_fee)
    }

    pub fn maybe_push(
        &mut self,
        env: Env,
        store: &mut dyn Storage,
        deposits_enabled: bool,
    ) -> ContractResult<Option<BuildingCheckpoint>> {
        // Increment the index. For the first checkpoint, leave the index at
        // zero.
        let checkpoints = DequeExtension::new(&self.queue_id);
        let mut index = self.index;
        if !checkpoints.is_empty(store)? {
            index += 1;
        }

        // Build the signatory set for the new checkpoint based on the current
        // validator set.
        let sigset = SignatorySet::from_validator_ctx(store, env, index)?;

        // Do not push if there are no validators in the signatory set.
        if sigset.possible_vp() == 0 {
            return Ok(None);
        }

        // Do not push if the signatory set does not have a quorum.
        if !sigset.has_quorum() {
            return Ok(None);
        }

        self.index = index;

        checkpoints.push_back(store, &Checkpoint::new(sigset)?)?;

        let mut building = self.building(store)?;
        building.deposits_enabled = deposits_enabled;
        self.set(store, self.index, &**building)?;

        Ok(Some(building))
    }

    /// The active signatory set, which is the signatory set for the `Building`
    /// checkpoint.
    pub fn active_sigset(&self, store: &dyn Storage) -> ContractResult<SignatorySet> {
        Ok(self.building(store)?.sigset.clone())
    }

    /// Process a batch of signatures, applying them to the checkpoint with the
    /// given index.
    ///
    /// Note that signatures can be sumitted to checkpoints which are already
    /// complete, causing them to be over-signed (which does not affect their
    /// validity). This is useful for letting all signers submit, regardless of
    /// whether they are faster or slower than the other signers. This is
    /// useful, for example, in being able to check if a signer is offline.
    ///
    /// If the batch of signatures causes the checkpoint to be fully signed, it
    /// will be advanced to the `Complete` state.
    ///
    /// This method is exempt from paying transaction fees since the amount of
    /// signatures that can be submitted is capped and this type of transaction
    /// cannot be used to DoS the network.
    pub fn sign(
        &mut self,
        store: &mut dyn Storage,
        xpub: &Xpub,
        sigs: Vec<Signature>,
        index: u32,
        btc_height: u32,
    ) -> ContractResult<()> {
        let mut checkpoint = self.get(store, index)?;
        let status = checkpoint.status.clone();
        if matches!(status, CheckpointStatus::Building) {
            return Err(ContractError::Checkpoint(
                "Checkpoint is still building".into(),
            ));
        }

        checkpoint.sign(xpub, sigs, btc_height)?;

        if matches!(status, CheckpointStatus::Signing) && checkpoint.signed() {
            let checkpoint_tx = checkpoint.checkpoint_tx()?;
            println!("Checkpoint signing complete {:?}", checkpoint_tx);
            checkpoint.advance();
            checkpoint.status = CheckpointStatus::Complete
        }

        self.set(store, index, &checkpoint)?;

        Ok(())
    }

    /// The signatory set for the checkpoint with the given index.
    pub fn sigset(&self, store: &dyn Storage, index: u32) -> ContractResult<SignatorySet> {
        Ok(self.get(store, index)?.sigset.clone())
    }

    /// Query building miner fee for checking with fee_collected
    pub fn query_building_miner_fee(
        &self,
        store: &dyn Storage,
        cp_index: u32,
        timestamping_commitment: [u8; 32],
    ) -> ContractResult<u64> {
        self.calc_fee_checkpoint(store, cp_index, &timestamping_commitment)
    }

    /// The number of completed checkpoints which have not yet been confirmed on
    /// the Bitcoin network.
    pub fn num_unconfirmed(&self, store: &dyn Storage) -> ContractResult<u32> {
        let has_signing = self.signing(store)?.is_some();
        let signing_offset = has_signing as u32;

        let last_completed_index = self.index.checked_sub(1 + signing_offset);
        let last_completed_index = match last_completed_index {
            None => return Ok(0),
            Some(index) => index,
        };

        let confirmed_index = match self.confirmed_index {
            None => return Ok(self.len(store)? - 1 - signing_offset),
            Some(index) => index,
        };

        Ok(last_completed_index - confirmed_index)
    }

    pub fn first_unconfirmed_index(&self, store: &dyn Storage) -> ContractResult<Option<u32>> {
        let num_unconf = self.num_unconfirmed(store)?;
        if num_unconf == 0 {
            return Ok(None);
        }

        let has_signing = self.signing(store)?.is_some();
        let signing_offset = has_signing as u32;

        Ok(Some(self.index - num_unconf - signing_offset))
    }

    pub fn unconfirmed(&self, store: &dyn Storage) -> ContractResult<Vec<Checkpoint>> {
        let first_unconf_index = self.first_unconfirmed_index(store)?;
        if let Some(index) = first_unconf_index {
            let mut out = vec![];
            for i in index..=self.index {
                let cp = self.get(store, i)?;
                if !matches!(cp.status, CheckpointStatus::Complete) {
                    break;
                }
                out.push(cp);
            }
            Ok(out)
        } else {
            Ok(vec![])
        }
    }

    pub fn unhandled_confirmed(&self, store: &dyn Storage) -> ContractResult<Vec<u32>> {
        if self.confirmed_index.is_none() {
            return Ok(vec![]);
        }

        let mut out = vec![];
        for i in self.first_unhandled_confirmed_cp_index..=self.confirmed_index.unwrap() {
            let cp = self.get(store, i)?;
            if !matches!(cp.status, CheckpointStatus::Complete) {
                println!("Existing confirmed checkpoint without 'complete' status.");
                break;
            }
            out.push(i);
        }
        Ok(out)
    }

    pub fn unconfirmed_fees_paid(&self, store: &dyn Storage) -> ContractResult<u64> {
        self.unconfirmed(store)?
            .iter()
            .map(|cp| cp.checkpoint_tx_miner_fees())
            .try_fold(0, |fees, result: ContractResult<_>| {
                let fee = result?;
                Ok::<_, ContractError>(fees + fee)
            })
    }

    pub fn unconfirmed_vbytes(
        &self,
        store: &dyn Storage,
        config: &CheckpointConfig,
    ) -> ContractResult<u64> {
        self.unconfirmed(store)?
            .iter()
            .map(|cp| cp.est_vsize(config, &[0; 32])) // TODO: shouldn't need to pass fixed length commitment to est_vsize
            .try_fold(0, |sum, result: ContractResult<_>| {
                let vbytes = result?;
                Ok::<_, ContractError>(sum + vbytes)
            })
    }

    fn fee_adjustment(
        &self,
        store: &dyn Storage,
        fee_rate: u64,
        config: &CheckpointConfig,
    ) -> ContractResult<u64> {
        let unconf_fees_paid = self.unconfirmed_fees_paid(store)?;
        let unconf_vbytes = self.unconfirmed_vbytes(store, config)?;
        Ok((unconf_vbytes * fee_rate).saturating_sub(unconf_fees_paid))
    }

    pub fn backfill(
        &mut self,
        store: &mut dyn Storage,
        first_index: u32,
        redeem_scripts: impl Iterator<Item = Script>,
        threshold_ratio: (u64, u64),
    ) -> ContractResult<()> {
        let mut index = first_index + 1;
        let checkpoints = self.queue();
        let create_time = checkpoints.get(store, 0)?.unwrap().create_time();

        for script in redeem_scripts {
            index -= 1;

            if index >= self.first_index(store)? {
                continue;
            }

            let (mut sigset, _) = SignatorySet::from_script(&script, threshold_ratio)?;
            sigset.index = index;
            sigset.create_time = create_time;
            let mut cp = Checkpoint::new(sigset)?;
            cp.status = CheckpointStatus::Complete;

            checkpoints.push_front(store, &cp)?;
        }

        Ok(())
    }
}

/// Takes a previous fee rate and returns a new fee rate, adjusted up or down by
/// 25%. The new fee rate is capped at the maximum and minimum fee rates
/// specified in the given config.
pub fn adjust_fee_rate(prev_fee_rate: u64, up: bool, config: &CheckpointConfig) -> u64 {
    if up {
        (prev_fee_rate * 5 / 4).max(prev_fee_rate + 1)
    } else {
        (prev_fee_rate * 3 / 4).min(prev_fee_rate - 1)
    }
    .min(config.max_fee_rate)
    .max(config.min_fee_rate)
}