// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

#![forbid(unsafe_code)]
#![allow(dead_code)]

#[cfg(test)]
mod executor_test;
#[cfg(test)]
mod mock_vm;

use anyhow::{bail, ensure, format_err, Result};
use lazy_static::lazy_static;
use libra_config::config::NodeConfig;
use libra_config::config::VMConfig;
use libra_crypto::{
    hash::{
        CryptoHash, EventAccumulatorHasher, TransactionAccumulatorHasher,
        ACCUMULATOR_PLACEHOLDER_HASH, GENESIS_BLOCK_ID, PRE_GENESIS_BLOCK_ID,
        SPARSE_MERKLE_PLACEHOLDER_HASH,
    },
    HashValue,
};
use libra_logger::prelude::*;
use libra_types::{
    account_address::AccountAddress,
    account_state_blob::AccountStateBlob,
    block_info::{BlockInfo, Round},
    contract_event::ContractEvent,
    crypto_proxies::LedgerInfoWithSignatures,
    crypto_proxies::ValidatorSet,
    ledger_info::LedgerInfo,
    proof::{accumulator::InMemoryAccumulator, definition::LeafCount, SparseMerkleProof},
    transaction::{
        Transaction, TransactionInfo, TransactionListWithProof, TransactionOutput,
        TransactionPayload, TransactionStatus, TransactionToCommit, Version,
    },
    write_set::{WriteOp, WriteSet},
};
use scratchpad::{ProofRead, SparseMerkleTree};
use serde::{Deserialize, Serialize};
use std::{
    collections::{hash_map, BTreeMap, HashMap, HashSet},
    convert::TryFrom,
    marker::PhantomData,
    sync::Arc,
};
use storage_client::{StorageRead, StorageWrite, VerifiedStateView};
use vm_runtime::VMExecutor;

lazy_static! {
    static ref OP_COUNTERS: libra_metrics::OpMetrics =
        libra_metrics::OpMetrics::new_and_registered("executor");
}

const GENESIS_EPOCH: u64 = 0;
const GENESIS_ROUND: Round = 0;

/// A structure that summarizes the result of the execution needed for consensus to agree on.
/// The execution is responsible for generating the ID of the new state, which is returned in the
/// result.
///
/// Not every transaction in the payload succeeds: the returned vector keeps the boolean status
/// of success / failure of the transactions.
/// Note that the specific details of compute_status are opaque to StateMachineReplication,
/// which is going to simply pass the results between StateComputer and TxnManager.
#[derive(Debug, Default, PartialEq, Eq, Clone)]
pub struct StateComputeResult {
    pub executed_state: ExecutedState,
    /// The compute status (success/failure) of the given payload. The specific details are opaque
    /// for StateMachineReplication, which is merely passing it between StateComputer and
    /// TxnManager.
    pub compute_status: Vec<TransactionStatus>,
}

impl StateComputeResult {
    pub fn version(&self) -> Version {
        self.executed_state.version
    }

    pub fn root_hash(&self) -> HashValue {
        self.executed_state.state_id
    }

    pub fn status(&self) -> &Vec<TransactionStatus> {
        &self.compute_status
    }

    pub fn has_reconfiguration(&self) -> bool {
        self.executed_state.validators.is_some()
    }
}

/// Executed state derived from StateComputeResult that is maintained with every proposed block.
/// `state_id`(transaction accumulator root hash) summarized both the information of the version and
/// the validators.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutedState {
    /// Tracks the execution state of a proposed block
    pub state_id: HashValue,
    /// Version of after executing a proposed block.  This state must be persisted to ensure
    /// that on restart that the version is calculated correctly
    pub version: Version,
    /// If set, this is the validator set that should be changed to if this block is committed.
    /// TODO [Reconfiguration] the validators are currently ignored, no reconfiguration yet.
    pub validators: Option<ValidatorSet>,
}

impl ExecutedState {
    pub fn state_for_genesis() -> Self {
        ExecutedState {
            state_id: *ACCUMULATOR_PLACEHOLDER_HASH,
            version: 0,
            validators: None,
        }
    }
}

/// The entire set of data associated with a transaction. In addition to the output generated by VM
/// which includes the write set and events, this also has the in-memory trees.
#[derive(Clone, Debug)]
pub struct TransactionData {
    /// Each entry in this map represents the new blob value of an account touched by this
    /// transaction. The blob is obtained by deserializing the previous blob into a BTreeMap,
    /// applying relevant portion of write set on the map and serializing the updated map into a
    /// new blob.
    account_blobs: HashMap<AccountAddress, AccountStateBlob>,

    /// The list of events emitted during this transaction.
    events: Vec<ContractEvent>,

    /// The execution status set by the VM.
    status: TransactionStatus,

    /// The in-memory Sparse Merkle Tree after the write set is applied. This is `Rc` because the
    /// tree has uncommitted state and sometimes `StateVersionView` needs to have a pointer to the
    /// tree so VM can read it.
    state_tree: Arc<SparseMerkleTree>,

    /// The in-memory Merkle Accumulator that has all events emitted by this transaction.
    event_tree: Arc<InMemoryAccumulator<EventAccumulatorHasher>>,

    /// The amount of gas used.
    gas_used: u64,

    /// The number of newly created accounts.
    num_account_created: usize,

    /// The transaction info hash if the VM status output was keep, None otherwise
    txn_info_hash: Option<HashValue>,
}

impl TransactionData {
    fn new(
        account_blobs: HashMap<AccountAddress, AccountStateBlob>,
        events: Vec<ContractEvent>,
        status: TransactionStatus,
        state_tree: Arc<SparseMerkleTree>,
        event_tree: Arc<InMemoryAccumulator<EventAccumulatorHasher>>,
        gas_used: u64,
        num_account_created: usize,
        txn_info_hash: Option<HashValue>,
    ) -> Self {
        TransactionData {
            account_blobs,
            events,
            status,
            state_tree,
            event_tree,
            gas_used,
            num_account_created,
            txn_info_hash,
        }
    }

    fn account_blobs(&self) -> &HashMap<AccountAddress, AccountStateBlob> {
        &self.account_blobs
    }

    fn events(&self) -> &[ContractEvent] {
        &self.events
    }

    fn status(&self) -> &TransactionStatus {
        &self.status
    }

    fn state_root_hash(&self) -> HashValue {
        self.state_tree.root_hash()
    }

    fn event_root_hash(&self) -> HashValue {
        self.event_tree.root_hash()
    }

    fn gas_used(&self) -> u64 {
        self.gas_used
    }

    fn num_account_created(&self) -> usize {
        self.num_account_created
    }

    fn prune_state_tree(&self) {
        self.state_tree.prune()
    }

    pub fn txn_info_hash(&self) -> Option<HashValue> {
        self.txn_info_hash
    }
}

/// Generated by processing VM's output.
#[derive(Debug, Clone)]
pub struct ProcessedVMOutput {
    /// The entire set of data associated with each transaction.
    transaction_data: Vec<TransactionData>,

    /// The in-memory Merkle Accumulator and state Sparse Merkle Tree after appending all the
    /// transactions in this set.
    executed_trees: ExecutedTrees,

    /// If set, this is the validator set that should be changed to if this block is committed.
    /// TODO [Reconfiguration] the validators are currently ignored, no reconfiguration yet.
    validators: Option<ValidatorSet>,
}

impl ProcessedVMOutput {
    pub fn new(
        transaction_data: Vec<TransactionData>,
        executed_trees: ExecutedTrees,
        validators: Option<ValidatorSet>,
    ) -> Self {
        ProcessedVMOutput {
            transaction_data,
            executed_trees,
            validators,
        }
    }

    pub fn transaction_data(&self) -> &[TransactionData] {
        &self.transaction_data
    }

    pub fn executed_trees(&self) -> &ExecutedTrees {
        &self.executed_trees
    }

    pub fn accu_root(&self) -> HashValue {
        self.executed_trees().txn_accumulator().root_hash()
    }

    pub fn version(&self) -> Option<Version> {
        self.executed_trees().version()
    }

    pub fn validators(&self) -> &Option<ValidatorSet> {
        &self.validators
    }

    // This method should only be called by tests.
    pub fn set_validators(&mut self, validator_set: ValidatorSet) {
        self.validators = Some(validator_set)
    }

    pub fn state_compute_result(&self) -> StateComputeResult {
        let num_leaves = self.executed_trees().txn_accumulator().num_leaves();
        let version = if num_leaves == 0 { 0 } else { num_leaves - 1 };
        StateComputeResult {
            // Now that we have the root hash and execution status we can send the response to
            // consensus.
            // TODO: The VM will support a special transaction to set the validators for the
            // next epoch that is part of a block execution.
            executed_state: ExecutedState {
                state_id: self.accu_root(),
                version,
                validators: self.validators.clone(),
            },
            compute_status: self
                .transaction_data()
                .iter()
                .map(|txn_data| txn_data.status())
                .cloned()
                .collect(),
        }
    }
}

/// `Executor` implements all functionalities the execution module needs to provide.
pub struct Executor<V> {
    /// Client to storage service.
    storage_read_client: Arc<dyn StorageRead>,
    storage_write_client: Arc<dyn StorageWrite>,

    /// Configuration for the VM. The block processor currently creates a new VM for each block.
    vm_config: VMConfig,

    phantom: PhantomData<V>,
}

impl<V> Executor<V>
where
    V: VMExecutor,
{
    /// Constructs an `Executor`.
    pub fn new(
        storage_read_client: Arc<dyn StorageRead>,
        storage_write_client: Arc<dyn StorageWrite>,
        config: &NodeConfig,
    ) -> Self {
        let mut executor = Executor {
            storage_read_client: storage_read_client.clone(),
            storage_write_client,
            vm_config: config.vm_config.clone(),
            phantom: PhantomData,
        };
        if storage_read_client
            .get_startup_info()
            .expect("Shouldn't fail")
            .is_none()
        {
            let genesis_txn = config
                .execution
                .genesis
                .as_ref()
                .expect("failed to load genesis transaction!")
                .clone();
            executor.init_genesis(genesis_txn);
        }
        executor
    }

    /// This is used when we start for the first time and the DB is completely empty. It will write
    /// necessary information to DB by committing the genesis transaction.
    fn init_genesis(&mut self, genesis_txn: Transaction) {
        let genesis_txns = vec![genesis_txn];

        // Create a block with genesis_txn being the only transaction. Execute it then commit it
        // immediately.
        // We create `PRE_GENESIS_BLOCK_ID` as the parent of the genesis block.
        let pre_genesis_trees = ExecutedTrees::new_empty();
        let output = self
            .execute_block(
                genesis_txns.clone(),
                &pre_genesis_trees,
                &pre_genesis_trees,
                *PRE_GENESIS_BLOCK_ID,
                *GENESIS_BLOCK_ID,
            )
            .expect("Failed to execute genesis block.");

        let root_hash = output.accu_root();
        let ledger_info = LedgerInfo::new(
            BlockInfo::new(
                GENESIS_EPOCH,
                GENESIS_ROUND,
                *PRE_GENESIS_BLOCK_ID,
                root_hash,
                0,
                0,
                output.validators().clone(),
            ),
            HashValue::zero(),
        );
        let ledger_info_with_sigs =
            LedgerInfoWithSignatures::new(ledger_info, /* signatures = */ BTreeMap::new());
        self.commit_blocks(
            vec![(genesis_txns, Arc::new(output))],
            ledger_info_with_sigs,
            &pre_genesis_trees,
        )
        .expect("Failed to commit genesis block.");
        info!("GENESIS transaction is committed.")
    }

    /// Executes a block.
    pub fn execute_block(
        &self,
        transactions: Vec<Transaction>,
        parent_trees: &ExecutedTrees,
        committed_trees: &ExecutedTrees,
        parent_id: HashValue,
        id: HashValue,
    ) -> Result<ProcessedVMOutput> {
        debug!(
            "Received request to execute block. Parent id: {:x}. Id: {:x}.",
            parent_id, id
        );

        let _timer = OP_COUNTERS.timer("block_execute_time_s");
        // Construct a StateView and pass the transactions to VM.
        let state_view = VerifiedStateView::new(
            Arc::clone(&self.storage_read_client),
            committed_trees.version(),
            committed_trees.state_root(),
            parent_trees.state_tree(),
        );

        let vm_outputs = {
            let _timer = OP_COUNTERS.timer("vm_execute_block_time_s");
            V::execute_block(transactions.clone(), &self.vm_config, &state_view)?
        };

        let status: Vec<_> = vm_outputs
            .iter()
            .map(TransactionOutput::status)
            .cloned()
            .collect();
        if !status.is_empty() {
            debug!("Execution status: {:?}", status);
        }

        let (account_to_btree, account_to_proof) = state_view.into();
        let output = Self::process_vm_outputs(
            account_to_btree,
            account_to_proof,
            &transactions,
            vm_outputs,
            parent_trees,
        )
        .map_err(|err| format_err!("Failed to execute block: {}", err))?;

        Ok(output)
    }

    /// Saves eligible blocks to persistent storage.
    /// If we have multiple blocks and not all of them have signatures, we may send them to storage
    /// in a few batches. For example, if we have
    /// ```text
    /// A <- B <- C <- D <- E
    /// ```
    /// and only `C` and `E` have signatures, we will send `A`, `B` and `C` in the first batch,
    /// then `D` and `E` later in the another batch.
    /// Commits a block and all its ancestors in a batch manner. Returns `Ok(())` if successful.
    pub fn commit_blocks(
        &self,
        blocks: Vec<(Vec<Transaction>, Arc<ProcessedVMOutput>)>,
        ledger_info_with_sigs: LedgerInfoWithSignatures,
        synced_trees: &ExecutedTrees,
    ) -> Result<()> {
        debug!(
            "Received request to commit block {:x}.",
            ledger_info_with_sigs.ledger_info().consensus_block_id()
        );
        let num_persistent_txns = synced_trees.txn_accumulator().num_leaves();

        // All transactions that need to go to storage. In the above example, this means all the
        // transactions in A, B and C whose status == TransactionStatus::Keep.
        // This must be done before calculate potential skipping of transactions in idempotent commit.
        let mut txns_to_keep = vec![];
        for (txn, txn_data) in blocks
            .iter()
            .map(|block| itertools::zip_eq(&block.0, block.1.transaction_data()))
            .flatten()
        {
            if let TransactionStatus::Keep(_) = txn_data.status() {
                txns_to_keep.push((
                    TransactionToCommit::new(
                        txn.clone(),
                        txn_data.account_blobs().clone(),
                        txn_data.events().to_vec(),
                        txn_data.gas_used(),
                        txn_data.status().vm_status().major_status,
                    ),
                    txn_data.num_account_created(),
                ));
            }
        }
        let num_txns_to_keep = txns_to_keep.len() as u64;

        let last_block = blocks
            .last()
            .expect("CommittableBlockBatch has at least 1 block.");

        // Check that the version in ledger info (computed by consensus) matches the version
        // computed by us.
        let version = ledger_info_with_sigs.ledger_info().version();
        let num_txns_in_speculative_accumulator =
            last_block.1.executed_trees().txn_accumulator().num_leaves();
        assert_eq!(
            version + 1,
            num_txns_in_speculative_accumulator as Version,
            "Number of transactions in ledger info ({}) does not match number of transactions \
             in accumulator ({}).",
            version + 1,
            num_txns_in_speculative_accumulator,
        );

        // Skip txns that are already committed to allow failures in state sync process.
        let first_version_to_keep = version + 1 - num_txns_to_keep;
        assert!(
            first_version_to_keep <= num_persistent_txns,
            "first_version {} in the blocks to commit cannot exceed # of committed txns: {}.",
            first_version_to_keep,
            num_persistent_txns
        );

        let num_txns_to_skip = num_persistent_txns - first_version_to_keep;
        let first_version_to_commit = first_version_to_keep + num_txns_to_skip;
        if num_txns_to_skip != 0 {
            info!(
                "The lastest committed/synced version: {}, the first version to keep in the batch: {}.\
                 Skipping the first {} transactions and start committing from version {}",
                num_persistent_txns - 1, /* latest persistent version */
                first_version_to_keep,
                num_txns_to_skip,
                first_version_to_commit
            );
        }

        // Skip duplicate txns that are already persistent.
        let (txns_to_commit, list_num_account_created): (Vec<_>, Vec<_>) = txns_to_keep
            .into_iter()
            .skip(num_txns_to_skip as usize)
            .unzip();

        let num_txns_to_commit = txns_to_commit.len() as u64;

        {
            let _timer = OP_COUNTERS.timer("storage_save_transactions_time_s");
            OP_COUNTERS.observe("storage_save_transactions.count", num_txns_to_commit as f64);
            assert_eq!(first_version_to_commit, version + 1 - num_txns_to_commit);
            self.storage_write_client.save_transactions(
                txns_to_commit,
                first_version_to_commit,
                Some(ledger_info_with_sigs.clone()),
            )?;
        }
        // Only bump the counter when the commit succeeds.
        OP_COUNTERS.inc_by("num_accounts", list_num_account_created.into_iter().sum());

        for block in blocks {
            for txn_data in block.1.transaction_data() {
                txn_data.prune_state_tree();
            }
        }
        // Now that the blocks are persisted successfully, we can reply to consensus
        Ok(())
    }

    /// Verifies the transactions based on the provided proofs and ledger info. If the transactions
    /// are valid, executes them and commits immediately if execution results match the proofs.
    pub fn execute_and_commit_chunk(
        &self,
        txn_list_with_proof: TransactionListWithProof,
        ledger_info_with_sigs: LedgerInfoWithSignatures,
        synced_trees: &mut ExecutedTrees,
    ) -> Result<()> {
        info!(
            "Local synced version: {}. First transaction version in request: {:?}. \
             Number of transactions in request: {}.",
            synced_trees.txn_accumulator().num_leaves() - 1,
            txn_list_with_proof.first_transaction_version,
            txn_list_with_proof.transactions.len(),
        );

        let (num_txns_to_skip, first_version) = Self::verify_chunk(
            &txn_list_with_proof,
            &ledger_info_with_sigs,
            synced_trees.txn_accumulator().num_leaves(),
        )?;

        info!("Skipping the first {} transactions.", num_txns_to_skip);
        let transactions: Vec<_> = txn_list_with_proof
            .transactions
            .into_iter()
            .skip(num_txns_to_skip as usize)
            .collect();

        // Construct a StateView and pass the transactions to VM.
        let state_view = VerifiedStateView::new(
            Arc::clone(&self.storage_read_client),
            synced_trees.version(),
            synced_trees.state_root(),
            synced_trees.state_tree(),
        );
        let vm_outputs = {
            let _timer = OP_COUNTERS.timer("vm_execute_chunk_time_s");
            V::execute_block(transactions.to_vec(), &self.vm_config, &state_view)?
        };

        // Since other validators have committed these transactions, their status should all be
        // TransactionStatus::Keep.
        for output in &vm_outputs {
            if let TransactionStatus::Discard(_) = output.status() {
                bail!("Syncing transactions that should be discarded.");
            }
        }

        let (account_to_btree, account_to_proof) = state_view.into();

        let output = Self::process_vm_outputs(
            account_to_btree,
            account_to_proof,
            &transactions,
            vm_outputs,
            synced_trees,
        )?;

        // Since we have verified the proofs, we just need to verify that each TransactionInfo
        // object matches what we have computed locally.
        let mut txns_to_commit = vec![];
        for (txn, txn_data) in itertools::zip_eq(transactions, output.transaction_data()) {
            txns_to_commit.push(TransactionToCommit::new(
                txn,
                txn_data.account_blobs().clone(),
                txn_data.events().to_vec(),
                txn_data.gas_used(),
                txn_data.status().vm_status().major_status,
            ));
        }

        // If this is the last chunk corresponding to this ledger info, send the ledger info to
        // storage.
        let ledger_info_to_commit = if synced_trees.txn_accumulator().num_leaves()
            + txns_to_commit.len() as LeafCount
            == ledger_info_with_sigs.ledger_info().version() + 1
        {
            ensure!(
                ledger_info_with_sigs
                    .ledger_info()
                    .transaction_accumulator_hash()
                    == output.executed_trees().txn_accumulator().root_hash(),
                "Root hash in ledger info does not match local computation."
            );
            Some(ledger_info_with_sigs)
        } else {
            // This means that the current chunk is not the last one. If it's empty, there's
            // nothing to write to storage. Since storage expect either new transaction or new
            // ledger info, we need to return here.
            if txns_to_commit.is_empty() {
                return Ok(());
            }
            None
        };
        self.storage_write_client.save_transactions(
            txns_to_commit,
            first_version,
            ledger_info_to_commit.clone(),
        )?;

        *synced_trees = output.executed_trees().clone();
        info!(
            "Synced to version {}.",
            synced_trees.version().expect("version must exist"),
        );

        if let Some(ledger_info_with_sigs) = ledger_info_to_commit {
            info!(
                "Synced to version {} with ledger info committed.",
                ledger_info_with_sigs.ledger_info().version()
            );
        }
        Ok(())
    }

    /// Verifies proofs using provided ledger info. Also verifies that the version of the first
    /// transaction matches the latest committed transaction. If the first few transaction happens
    /// to be older, returns how many need to be skipped and the first version to be committed.
    fn verify_chunk(
        txn_list_with_proof: &TransactionListWithProof,
        ledger_info_with_sigs: &LedgerInfoWithSignatures,
        num_committed_txns: u64,
    ) -> Result<(LeafCount, Version)> {
        txn_list_with_proof.verify(
            ledger_info_with_sigs.ledger_info(),
            txn_list_with_proof.first_transaction_version,
        )?;

        if txn_list_with_proof.transactions.is_empty() {
            return Ok((0, num_committed_txns as Version /* first_version */));
        }

        let first_txn_version = txn_list_with_proof
            .first_transaction_version
            .expect("first_transaction_version should exist.")
            as Version;

        ensure!(
            first_txn_version <= num_committed_txns,
            "Transaction list too new. Expected version: {}. First transaction version: {}.",
            num_committed_txns,
            first_txn_version
        );
        Ok((
            num_committed_txns - first_txn_version,
            num_committed_txns as Version,
        ))
    }

    /// Post-processing of what the VM outputs. Returns the entire block's output.
    fn process_vm_outputs(
        mut account_to_btree: HashMap<AccountAddress, BTreeMap<Vec<u8>, Vec<u8>>>,
        account_to_proof: HashMap<HashValue, SparseMerkleProof>,
        transactions: &[Transaction],
        vm_outputs: Vec<TransactionOutput>,
        parent_trees: &ExecutedTrees,
    ) -> Result<ProcessedVMOutput> {
        // The data of each individual transaction. For convenience purpose, even for the
        // transactions that will be discarded, we will compute its in-memory Sparse Merkle Tree
        // (it will be identical to the previous one).
        let mut txn_data = vec![];
        let mut current_state_tree = Arc::clone(parent_trees.state_tree());
        // The hash of each individual TransactionInfo object. This will not include the
        // transactions that will be discarded, since they do not go into the transaction
        // accumulator.
        let mut txn_info_hashes = vec![];
        let mut next_validator_set = None;

        let proof_reader = ProofReader::new(account_to_proof);
        for (vm_output, txn) in itertools::zip_eq(vm_outputs.into_iter(), transactions.iter()) {
            let (blobs, state_tree, num_accounts_created) = Self::process_write_set(
                txn,
                &mut account_to_btree,
                &proof_reader,
                vm_output.write_set().clone(),
                &current_state_tree,
            )?;

            let event_tree = {
                let event_hashes: Vec<_> =
                    vm_output.events().iter().map(CryptoHash::hash).collect();
                InMemoryAccumulator::<EventAccumulatorHasher>::from_leaves(&event_hashes)
            };
            let mut txn_info_hash = None;

            match vm_output.status() {
                TransactionStatus::Keep(status) => {
                    ensure!(
                        !vm_output.write_set().is_empty(),
                        "Transaction with empty write set should be discarded.",
                    );
                    // Compute hash for the TransactionInfo object. We need the hash of the
                    // transaction itself, the state root hash as well as the event root hash.
                    let txn_info = TransactionInfo::new(
                        txn.hash(),
                        state_tree.root_hash(),
                        event_tree.root_hash(),
                        vm_output.gas_used(),
                        status.major_status,
                    );

                    let real_txn_info_hash = txn_info.hash();
                    txn_info_hashes.push(real_txn_info_hash);
                    txn_info_hash = Some(real_txn_info_hash);
                }
                TransactionStatus::Discard(_) => {
                    ensure!(
                        vm_output.write_set().is_empty(),
                        "Discarded transaction has non-empty write set.",
                    );
                    ensure!(
                        vm_output.events().is_empty(),
                        "Discarded transaction has non-empty events.",
                    );
                }
            }

            txn_data.push(TransactionData::new(
                blobs,
                vm_output.events().to_vec(),
                vm_output.status().clone(),
                Arc::clone(&state_tree),
                Arc::new(event_tree),
                vm_output.gas_used(),
                num_accounts_created,
                txn_info_hash,
            ));
            current_state_tree = state_tree;

            // check for change in validator set
            let validator_set_change_event_key = ValidatorSet::change_event_key();
            for event in vm_output.events() {
                if *event.key() == validator_set_change_event_key {
                    next_validator_set = Some(ValidatorSet::from_bytes(event.event_data())?);
                    break;
                }
            }
        }

        let current_transaction_accumulator = parent_trees
            .transaction_accumulator
            .append(&txn_info_hashes);
        Ok(ProcessedVMOutput::new(
            txn_data,
            ExecutedTrees {
                state_tree: current_state_tree,
                transaction_accumulator: Arc::new(current_transaction_accumulator),
            },
            next_validator_set,
        ))
    }

    /// For all accounts modified by this transaction, find the previous blob and update it based
    /// on the write set. Returns the blob value of all these accounts as well as the newly
    /// constructed state tree.
    fn process_write_set(
        transaction: &Transaction,
        account_to_btree: &mut HashMap<AccountAddress, BTreeMap<Vec<u8>, Vec<u8>>>,
        proof_reader: &ProofReader,
        write_set: WriteSet,
        previous_state_tree: &SparseMerkleTree,
    ) -> Result<(
        HashMap<AccountAddress, AccountStateBlob>,
        Arc<SparseMerkleTree>,
        usize, /* num_account_created */
    )> {
        let mut updated_blobs = HashMap::new();
        let mut num_accounts_created = 0;

        // Find all addresses this transaction touches while processing each write op.
        let mut addrs = HashSet::new();
        for (access_path, write_op) in write_set.into_iter() {
            let address = access_path.address;
            let path = access_path.path;
            match account_to_btree.entry(address) {
                hash_map::Entry::Occupied(mut entry) => {
                    let account_btree = entry.get_mut();
                    // TODO(gzh): we check account creation here for now. Will remove it once we
                    // have a better way.
                    if account_btree.is_empty() {
                        num_accounts_created += 1;
                    }
                    Self::update_account_btree(account_btree, path, write_op);
                }
                hash_map::Entry::Vacant(entry) => {
                    // Before writing to an account, VM should always read that account. So we
                    // should not reach this code path. The exception is genesis transaction (and
                    // maybe other FTVM transactions).
                    match transaction.as_signed_user_txn()?.payload() {
                        TransactionPayload::Program
                        | TransactionPayload::Module(_)
                        | TransactionPayload::Script(_) => {
                            bail!("Write set should be a subset of read set.")
                        }
                        TransactionPayload::WriteSet(_) => (),
                    }

                    let mut account_btree = BTreeMap::new();
                    Self::update_account_btree(&mut account_btree, path, write_op);
                    entry.insert(account_btree);
                }
            }
            addrs.insert(address);
        }

        for addr in addrs {
            let account_btree = account_to_btree.get(&addr).expect("Address should exist.");
            let account_blob = AccountStateBlob::try_from(account_btree)?;
            updated_blobs.insert(addr, account_blob);
        }
        let state_tree = Arc::new(
            previous_state_tree
                .update(
                    updated_blobs
                        .iter()
                        .map(|(addr, value)| (addr.hash(), value.clone()))
                        .collect(),
                    proof_reader,
                )
                .expect("Failed to update state tree."),
        );

        Ok((updated_blobs, state_tree, num_accounts_created))
    }

    fn update_account_btree(
        account_btree: &mut BTreeMap<Vec<u8>, Vec<u8>>,
        path: Vec<u8>,
        write_op: WriteOp,
    ) {
        match write_op {
            WriteOp::Value(new_value) => account_btree.insert(path, new_value),
            WriteOp::Deletion => account_btree.remove(&path),
        };
    }
}

#[derive(Clone, Debug)]
pub struct ExecutedTrees {
    /// The in-memory Sparse Merkle Tree representing a specific state after execution. If this
    /// tree is presenting the latest commited state, it will have a single Subtree node (or
    /// Empty node) whose hash equals the root hash of the newest Sparse Merkle Tree in
    /// storage.
    state_tree: Arc<SparseMerkleTree>,

    /// The in-memory Merkle Accumulator representing a blockchain state consistent with the
    /// `state_tree`.
    transaction_accumulator: Arc<InMemoryAccumulator<TransactionAccumulatorHasher>>,
}

impl ExecutedTrees {
    pub fn state_tree(&self) -> &Arc<SparseMerkleTree> {
        &self.state_tree
    }

    pub fn txn_accumulator(&self) -> &Arc<InMemoryAccumulator<TransactionAccumulatorHasher>> {
        &self.transaction_accumulator
    }

    pub fn version(&self) -> Option<Version> {
        let num_elements = self.txn_accumulator().num_leaves() as u64;
        if num_elements > 0 {
            Some(num_elements - 1)
        } else {
            None
        }
    }

    pub fn state_id(&self) -> HashValue {
        self.txn_accumulator().root_hash()
    }

    pub fn state_root(&self) -> HashValue {
        self.state_tree().root_hash()
    }

    pub fn new(
        state_root_hash: HashValue,
        frozen_subtrees_in_accumulator: Vec<HashValue>,
        num_leaves_in_accumulator: u64,
    ) -> ExecutedTrees {
        ExecutedTrees {
            state_tree: Arc::new(SparseMerkleTree::new(state_root_hash)),
            transaction_accumulator: Arc::new(
                InMemoryAccumulator::new(frozen_subtrees_in_accumulator, num_leaves_in_accumulator)
                    .expect("The startup info read from storage should be valid."),
            ),
        }
    }

    pub fn new_empty() -> ExecutedTrees {
        Self::new(*SPARSE_MERKLE_PLACEHOLDER_HASH, vec![], 0)
    }
}

struct ProofReader {
    account_to_proof: HashMap<HashValue, SparseMerkleProof>,
}

impl ProofReader {
    fn new(account_to_proof: HashMap<HashValue, SparseMerkleProof>) -> Self {
        ProofReader { account_to_proof }
    }
}

impl ProofRead for ProofReader {
    fn get_proof(&self, key: HashValue) -> Option<&SparseMerkleProof> {
        self.account_to_proof.get(&key)
    }
}
