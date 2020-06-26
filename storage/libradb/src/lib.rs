// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

#![forbid(unsafe_code)]

//! This crate provides [`LibraDB`] which represents physical storage of the core Libra data
//! structures.
//!
//! It relays read/write operations on the physical storage via [`schemadb`] to the underlying
//! Key-Value storage system, and implements libra data structures on top of it.

// Used in this and other crates for testing.
#[cfg(any(test, feature = "fuzzing"))]
pub mod test_helper;

pub mod backup;
pub mod errors;
pub mod schema;

mod change_set;
mod event_store;
mod ledger_counters;
mod ledger_store;
mod pruner;
mod state_store;
mod system_store;
mod transaction_store;

#[cfg(any(test, feature = "fuzzing"))]
#[allow(dead_code)]
mod libradb_test;

#[cfg(feature = "fuzzing")]
pub use libradb_test::test_save_blocks_impl;

use crate::{
    backup::backup_handler::BackupHandler,
    change_set::{ChangeSet, SealedChangeSet},
    errors::LibraDbError,
    event_store::EventStore,
    ledger_counters::LedgerCounters,
    ledger_store::LedgerStore,
    pruner::Pruner,
    schema::*,
    state_store::StateStore,
    system_store::SystemStore,
    transaction_store::TransactionStore,
};
use anyhow::{ensure, Result};
use itertools::{izip, zip_eq};
use jellyfish_merkle::{restore::JellyfishMerkleRestore, TreeReader, TreeWriter};
use libra_crypto::hash::{CryptoHash, HashValue, SPARSE_MERKLE_PLACEHOLDER_HASH};
use libra_logger::prelude::*;
use libra_metrics::{
    register_int_counter, register_int_gauge, register_int_gauge_vec, IntCounter, IntGauge,
    IntGaugeVec, OpMetrics,
};
use libra_types::{
    account_address::AccountAddress,
    account_state_blob::{AccountStateBlob, AccountStateWithProof},
    contract_event::{ContractEvent, EventWithProof},
    epoch_change::EpochChangeProof,
    event::EventKey,
    ledger_info::LedgerInfoWithSignatures,
    proof::{
        AccountStateProof, AccumulatorConsistencyProof, EventProof, SparseMerkleProof,
        SparseMerkleRangeProof, TransactionListProof,
    },
    transaction::{
        TransactionInfo, TransactionListWithProof, TransactionToCommit, TransactionWithProof,
        Version, PRE_GENESIS_VERSION,
    },
};
use once_cell::sync::Lazy;
use schemadb::{DB, DEFAULT_CF_NAME};
use std::{iter::Iterator, path::Path, sync::Arc, time::Instant};
use storage_interface::{DbReader, DbWriter, StartupInfo, TreeState};

static OP_COUNTER: Lazy<OpMetrics> = Lazy::new(|| OpMetrics::new_and_registered("storage"));

pub static LIBRA_STORAGE_CF_SIZE_BYTES: Lazy<IntGaugeVec> = Lazy::new(|| {
    register_int_gauge_vec!(
        // metric name
        "libra_storage_cf_size_bytes",
        // metric description
        "Libra storage Column Family size in bytes",
        // metric labels (dimensions)
        &["cf_name"]
    )
    .unwrap()
});

pub static LIBRA_STORAGE_COMMITTED_TXNS: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!(
        "libra_storage_committed_txns",
        "Libra storage committed transactions"
    )
    .unwrap()
});

pub static LIBRA_STORAGE_LATEST_TXN_VERSION: Lazy<IntGauge> = Lazy::new(|| {
    register_int_gauge!(
        "libra_storage_latest_transaction_version",
        "Libra storage latest transaction version"
    )
    .unwrap()
});

const MAX_LIMIT: u64 = 1000;

// TODO: Either implement an iteration API to allow a very old client to loop through a long history
// or guarantee that there is always a recent enough waypoint and client knows to boot from there.
const MAX_NUM_EPOCH_ENDING_LEDGER_INFO: usize = 100;

fn error_if_too_many_requested(num_requested: u64, max_allowed: u64) -> Result<()> {
    if num_requested > max_allowed {
        Err(LibraDbError::TooManyRequested(num_requested, max_allowed).into())
    } else {
        Ok(())
    }
}

/// This holds a handle to the underlying DB responsible for physical storage and provides APIs for
/// access to the core Libra data structures.
pub struct LibraDB {
    db: Arc<DB>,
    ledger_store: Arc<LedgerStore>,
    transaction_store: Arc<TransactionStore>,
    state_store: Arc<StateStore>,
    event_store: EventStore,
    system_store: SystemStore,
    pruner: Option<Pruner>,
}

impl LibraDB {
    pub fn open<P: AsRef<Path> + Clone>(
        db_root_path: P,
        readonly: bool,
        prune_window: Option<u64>,
    ) -> Result<Self> {
        let column_families = vec![
            /* LedgerInfo CF = */ DEFAULT_CF_NAME,
            EPOCH_BY_VERSION_CF_NAME,
            EVENT_ACCUMULATOR_CF_NAME,
            EVENT_BY_KEY_CF_NAME,
            EVENT_CF_NAME,
            JELLYFISH_MERKLE_NODE_CF_NAME,
            LEDGER_COUNTERS_CF_NAME,
            STALE_NODE_INDEX_CF_NAME,
            TRANSACTION_CF_NAME,
            TRANSACTION_ACCUMULATOR_CF_NAME,
            TRANSACTION_BY_ACCOUNT_CF_NAME,
            TRANSACTION_INFO_CF_NAME,
        ];

        let path = db_root_path.as_ref().join("libradb");
        let instant = Instant::now();

        let db = Arc::new(if readonly {
            DB::open_readonly(path.clone(), "libradb_ro", column_families)?
        } else {
            DB::open(path.clone(), "libradb", column_families)?
        });

        info!(
            "Opened LibraDB at {:?} in {} ms",
            path,
            instant.elapsed().as_millis()
        );

        Ok(LibraDB {
            db: Arc::clone(&db),
            event_store: EventStore::new(Arc::clone(&db)),
            ledger_store: Arc::new(LedgerStore::new(Arc::clone(&db))),
            state_store: Arc::new(StateStore::new(Arc::clone(&db))),
            transaction_store: Arc::new(TransactionStore::new(Arc::clone(&db))),
            system_store: SystemStore::new(Arc::clone(&db)),
            pruner: prune_window.map(|n| Pruner::new(Arc::clone(&db), n)),
        })
    }

    /// This opens db in non-readonly mode, without the pruner.
    #[cfg(any(test, feature = "fuzzing"))]
    pub fn new_for_test<P: AsRef<Path> + Clone>(db_root_path: P) -> Self {
        Self::open(
            db_root_path,
            false, /* readonly */
            None,  /* pruner */
        )
        .expect("Unable to open LibraDB")
    }

    // ================================== Public API ==================================

    /// Returns ledger infos reflecting epoch bumps starting with the given epoch. If there are no
    /// more than `MAX_NUM_EPOCH_ENDING_LEDGER_INFO` results, this function returns all of them,
    /// otherwise the first `MAX_NUM_EPOCH_ENDING_LEDGER_INFO` results are returned and a flag
    /// (when true) will be used to indicate the fact that there is more.
    pub fn get_epoch_ending_ledger_infos(
        &self,
        start_epoch: u64,
        end_epoch: u64,
    ) -> Result<(Vec<LedgerInfoWithSignatures>, bool)> {
        self.get_epoch_ending_ledger_infos_impl(
            start_epoch,
            end_epoch,
            MAX_NUM_EPOCH_ENDING_LEDGER_INFO,
        )
    }

    fn get_epoch_ending_ledger_infos_impl(
        &self,
        start_epoch: u64,
        end_epoch: u64,
        limit: usize,
    ) -> Result<(Vec<LedgerInfoWithSignatures>, bool)> {
        ensure!(
            start_epoch <= end_epoch,
            "Bad epoch range [{}, {})",
            start_epoch,
            end_epoch,
        );
        // Note that the latest epoch can be the same with the current epoch (in most cases), or
        // current_epoch + 1 (when the latest ledger_info carries next validator set)
        let latest_epoch = self
            .ledger_store
            .get_latest_ledger_info()?
            .ledger_info()
            .next_block_epoch();
        ensure!(
            end_epoch <= latest_epoch,
            "Unable to provide epoch change ledger info for still open epoch. asked upper bound: {}, last sealed epoch: {}",
            end_epoch,
            latest_epoch - 1,  // okay to -1 because genesis LedgerInfo has .next_block_epoch() == 1
        );

        let (paging_epoch, more) = if end_epoch - start_epoch > limit as u64 {
            (start_epoch + limit as u64, true)
        } else {
            (end_epoch, false)
        };

        let lis = self
            .ledger_store
            .get_epoch_ending_ledger_info_iter(start_epoch, paging_epoch)?
            .collect::<Result<Vec<_>>>()?;
        ensure!(
            lis.len() == (paging_epoch - start_epoch) as usize,
            "DB corruption: missing epoch ending ledger info for epoch {}",
            lis.last()
                .map(|li| li.ledger_info().next_block_epoch())
                .unwrap_or(start_epoch),
        );
        Ok((lis, more))
    }

    pub fn get_transaction_with_proof(
        &self,
        version: Version,
        ledger_version: Version,
        fetch_events: bool,
    ) -> Result<TransactionWithProof> {
        let proof = self
            .ledger_store
            .get_transaction_info_with_proof(version, ledger_version)?;
        let transaction = self.transaction_store.get_transaction(version)?;

        // If events were requested, also fetch those.
        let events = if fetch_events {
            Some(self.event_store.get_events_by_version(version)?)
        } else {
            None
        };

        Ok(TransactionWithProof {
            version,
            transaction,
            events,
            proof,
        })
    }

    // ================================== Backup APIs ===================================

    /// Gets an instance of `BackupHandler` for data backup purpose.
    pub fn get_backup_handler(&self) -> BackupHandler {
        BackupHandler::new(
            Arc::clone(&self.ledger_store),
            Arc::clone(&self.transaction_store),
            Arc::clone(&self.state_store),
        )
    }

    pub fn restore_account_state(
        &self,
        iter: impl Iterator<Item = (Vec<(HashValue, AccountStateBlob)>, SparseMerkleRangeProof)>,
        version: Version,
        expected_root_hash: HashValue,
    ) -> Result<()> {
        let mut restore =
            JellyfishMerkleRestore::new(&*self.state_store, version, expected_root_hash)?;
        for (chunk, proof) in iter {
            restore.add_chunk(chunk, proof)?;
        }
        restore.finish()?;
        Ok(())
    }

    pub fn get_state_restore_receiver(
        &self,
        version: Version,
        expected_root_hash: HashValue,
    ) -> Result<JellyfishMerkleRestore<impl TreeReader + TreeWriter>> {
        JellyfishMerkleRestore::new(&*self.state_store, version, expected_root_hash)
    }

    // ================================== Private APIs ==================================
    fn get_events_by_event_key(
        &self,
        event_key: &EventKey,
        start_seq_num: u64,
        ascending: bool,
        limit: u64,
        ledger_version: Version,
    ) -> Result<Vec<EventWithProof>> {
        error_if_too_many_requested(limit, MAX_LIMIT)?;
        let get_latest = !ascending && start_seq_num == u64::max_value();

        let cursor = if get_latest {
            // Caller wants the latest, figure out the latest seq_num.
            // In the case of no events on that path, use 0 and expect empty result below.
            self.event_store
                .get_latest_sequence_number(ledger_version, &event_key)?
                .unwrap_or(0)
        } else {
            start_seq_num
        };

        // Convert requested range and order to a range in ascending order.
        let (first_seq, real_limit) = get_first_seq_num_and_limit(ascending, cursor, limit)?;

        // Query the index.
        let mut event_keys = self.event_store.lookup_events_by_key(
            &event_key,
            first_seq,
            real_limit,
            ledger_version,
        )?;

        // When descending, it's possible that user is asking for something beyond the latest
        // sequence number, in which case we will consider it a bad request and return an empty
        // list.
        // For example, if the latest sequence number is 100, and the caller is asking for 110 to
        // 90, we will get 90 to 100 from the index lookup above. Seeing that the last item
        // is 100 instead of 110 tells us 110 is out of bound.
        if !ascending {
            if let Some((seq_num, _, _)) = event_keys.last() {
                if *seq_num < cursor {
                    event_keys = Vec::new();
                }
            }
        }

        let mut events_with_proof = event_keys
            .into_iter()
            .map(|(seq, ver, idx)| {
                let (event, event_proof) = self
                    .event_store
                    .get_event_with_proof_by_version_and_index(ver, idx)?;
                ensure!(
                    seq == event.sequence_number(),
                    "Index broken, expected seq:{}, actual:{}",
                    seq,
                    event.sequence_number()
                );
                let txn_info_with_proof = self
                    .ledger_store
                    .get_transaction_info_with_proof(ver, ledger_version)?;
                let proof = EventProof::new(txn_info_with_proof, event_proof);
                Ok(EventWithProof::new(ver, idx, event, proof))
            })
            .collect::<Result<Vec<_>>>()?;
        if !ascending {
            events_with_proof.reverse();
        }

        Ok(events_with_proof)
    }

    /// Convert a `ChangeSet` to `SealedChangeSet`.
    ///
    /// Specifically, counter increases are added to current counter values and converted to DB
    /// alternations.
    fn seal_change_set(
        &self,
        first_version: Version,
        num_txns: Version,
        mut cs: ChangeSet,
    ) -> Result<(SealedChangeSet, Option<LedgerCounters>)> {
        // Avoid reading base counter values when not necessary.
        let counters = if num_txns > 0 {
            Some(self.system_store.bump_ledger_counters(
                first_version,
                first_version + num_txns - 1,
                cs.counter_bumps,
                &mut cs.batch,
            )?)
        } else {
            None
        };

        Ok((SealedChangeSet { batch: cs.batch }, counters))
    }

    fn save_transactions_impl(
        &self,
        txns_to_commit: &[TransactionToCommit],
        first_version: u64,
        mut cs: &mut ChangeSet,
    ) -> Result<HashValue> {
        let last_version = first_version + txns_to_commit.len() as u64 - 1;

        // Account state updates. Gather account state root hashes
        let account_state_sets = txns_to_commit
            .iter()
            .map(|txn_to_commit| txn_to_commit.account_states().clone())
            .collect::<Vec<_>>();
        let state_root_hashes =
            self.state_store
                .put_account_state_sets(account_state_sets, first_version, &mut cs)?;

        // Event updates. Gather event accumulator root hashes.
        let event_root_hashes = zip_eq(first_version..=last_version, txns_to_commit)
            .map(|(ver, txn_to_commit)| {
                self.event_store
                    .put_events(ver, txn_to_commit.events(), &mut cs)
            })
            .collect::<Result<Vec<_>>>()?;

        // Transaction updates. Gather transaction hashes.
        zip_eq(first_version..=last_version, txns_to_commit)
            .map(|(ver, txn_to_commit)| {
                self.transaction_store
                    .put_transaction(ver, txn_to_commit.transaction(), &mut cs)
            })
            .collect::<Result<()>>()?;

        // Transaction accumulator updates. Get result root hash.
        let txn_infos = izip!(txns_to_commit, state_root_hashes, event_root_hashes)
            .map(|(t, s, e)| {
                Ok(TransactionInfo::new(
                    t.transaction().hash(),
                    s,
                    e,
                    t.gas_used(),
                    t.major_status(),
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        assert_eq!(txn_infos.len(), txns_to_commit.len());

        let new_root_hash =
            self.ledger_store
                .put_transaction_infos(first_version, &txn_infos, &mut cs)?;

        Ok(new_root_hash)
    }

    /// Write the whole schema batch including all data necessary to mutate the ledger
    /// state of some transaction by leveraging rocksdb atomicity support. Also committed are the
    /// LedgerCounters.
    fn commit(&self, sealed_cs: SealedChangeSet) -> Result<()> {
        self.db.write_schemas(sealed_cs.batch)?;

        match self.db.get_approximate_sizes_cf() {
            Ok(cf_sizes) => {
                for (cf_name, size) in cf_sizes {
                    OP_COUNTER.set(&format!("cf_size_bytes_{}", cf_name), size as usize);
                    LIBRA_STORAGE_CF_SIZE_BYTES
                        .with_label_values(&[&cf_name])
                        .set(size as i64);
                }
            }
            Err(err) => warn!(
                "Failed to get approximate size of column families: {}.",
                err
            ),
        }

        Ok(())
    }

    fn wake_pruner(&self, latest_version: Version) {
        if let Some(pruner) = self.pruner.as_ref() {
            pruner.wake(latest_version)
        }
    }
}

impl DbReader for LibraDB {
    fn get_epoch_ending_ledger_infos(
        &self,
        start_epoch: u64,
        end_epoch: u64,
    ) -> Result<EpochChangeProof> {
        let (ledger_info_with_sigs, more) =
            Self::get_epoch_ending_ledger_infos(&self, start_epoch, end_epoch)?;
        Ok(EpochChangeProof::new(ledger_info_with_sigs, more))
    }

    fn get_latest_account_state(
        &self,
        address: AccountAddress,
    ) -> Result<Option<AccountStateBlob>> {
        let ledger_info_with_sigs = self.ledger_store.get_latest_ledger_info()?;
        let version = ledger_info_with_sigs.ledger_info().version();
        let (blob, _proof) = self
            .state_store
            .get_account_state_with_proof_by_version(address, version)?;
        Ok(blob)
    }

    fn get_latest_ledger_info(&self) -> Result<LedgerInfoWithSignatures> {
        self.ledger_store.get_latest_ledger_info()
    }

    /// Returns a transaction that is the `seq_num`-th one associated with the given account. If
    /// the transaction with given `seq_num` doesn't exist, returns `None`.
    fn get_txn_by_account(
        &self,
        address: AccountAddress,
        seq_num: u64,
        ledger_version: Version,
        fetch_events: bool,
    ) -> Result<Option<TransactionWithProof>> {
        self.transaction_store
            .lookup_transaction_by_account(address, seq_num, ledger_version)?
            .map(|version| self.get_transaction_with_proof(version, ledger_version, fetch_events))
            .transpose()
    }

    // ======================= State Synchronizer Internal APIs ===================================
    /// Gets a batch of transactions for the purpose of synchronizing state to another node.
    ///
    /// This is used by the State Synchronizer module internally.
    fn get_transactions(
        &self,
        start_version: Version,
        limit: u64,
        ledger_version: Version,
        fetch_events: bool,
    ) -> Result<TransactionListWithProof> {
        error_if_too_many_requested(limit, MAX_LIMIT)?;

        if start_version > ledger_version || limit == 0 {
            return Ok(TransactionListWithProof::new_empty());
        }

        let limit = std::cmp::min(limit, ledger_version - start_version + 1);

        let txns = (start_version..start_version + limit)
            .map(|version| Ok(self.transaction_store.get_transaction(version)?))
            .collect::<Result<Vec<_>>>()?;
        let txn_infos = (start_version..start_version + limit)
            .map(|version| Ok(self.ledger_store.get_transaction_info(version)?))
            .collect::<Result<Vec<_>>>()?;
        let events = if fetch_events {
            Some(
                (start_version..start_version + limit)
                    .map(|version| Ok(self.event_store.get_events_by_version(version)?))
                    .collect::<Result<Vec<_>>>()?,
            )
        } else {
            None
        };
        let proof = TransactionListProof::new(
            self.ledger_store.get_transaction_range_proof(
                Some(start_version),
                limit,
                ledger_version,
            )?,
            txn_infos,
        );

        Ok(TransactionListWithProof::new(
            txns,
            events,
            Some(start_version),
            proof,
        ))
    }

    fn get_events(
        &self,
        event_key: &EventKey,
        start: u64,
        ascending: bool,
        limit: u64,
    ) -> Result<Vec<(u64, ContractEvent)>> {
        let version = self
            .ledger_store
            .get_latest_ledger_info()?
            .ledger_info()
            .version();
        let events = self
            .get_events_by_event_key(event_key, start, ascending, limit, version)?
            .into_iter()
            .map(|e| (e.transaction_version, e.event))
            .collect();
        Ok(events)
    }

    /// Gets ledger info at specified version and ensures it's an epoch change.
    fn get_epoch_ending_ledger_info(&self, version: u64) -> Result<LedgerInfoWithSignatures> {
        self.ledger_store.get_epoch_ending_ledger_info(version)
    }

    fn get_state_proof_with_ledger_info(
        &self,
        known_version: u64,
        ledger_info_with_sigs: LedgerInfoWithSignatures,
    ) -> Result<(EpochChangeProof, AccumulatorConsistencyProof)> {
        let ledger_info = ledger_info_with_sigs.ledger_info();
        let known_epoch = self.ledger_store.get_epoch(known_version)?;
        let epoch_change_proof = if known_epoch < ledger_info.next_block_epoch() {
            let (ledger_infos_with_sigs, more) =
                self.get_epoch_ending_ledger_infos(known_epoch, ledger_info.next_block_epoch())?;
            EpochChangeProof::new(ledger_infos_with_sigs, more)
        } else {
            EpochChangeProof::new(vec![], /* more = */ false)
        };

        let ledger_consistency_proof = self
            .ledger_store
            .get_consistency_proof(known_version, ledger_info.version())?;
        Ok((epoch_change_proof, ledger_consistency_proof))
    }

    fn get_state_proof(
        &self,
        known_version: u64,
    ) -> Result<(
        LedgerInfoWithSignatures,
        EpochChangeProof,
        AccumulatorConsistencyProof,
    )> {
        let ledger_info_with_sigs = self.ledger_store.get_latest_ledger_info()?;
        let (epoch_change_proof, ledger_consistency_proof) =
            self.get_state_proof_with_ledger_info(known_version, ledger_info_with_sigs.clone())?;
        Ok((
            ledger_info_with_sigs,
            epoch_change_proof,
            ledger_consistency_proof,
        ))
    }

    fn get_account_state_with_proof(
        &self,
        address: AccountAddress,
        version: Version,
        ledger_version: Version,
    ) -> Result<AccountStateWithProof> {
        ensure!(
            version <= ledger_version,
            "The queried version {} should be equal to or older than ledger version {}.",
            version,
            ledger_version
        );
        let latest_version = self.get_latest_version()?;
        ensure!(
            ledger_version <= latest_version,
            "The ledger version {} is greater than the latest version currently in ledger: {}",
            ledger_version,
            latest_version
        );

        let txn_info_with_proof = self
            .ledger_store
            .get_transaction_info_with_proof(version, ledger_version)?;
        let (account_state_blob, sparse_merkle_proof) = self
            .state_store
            .get_account_state_with_proof_by_version(address, version)?;
        Ok(AccountStateWithProof::new(
            version,
            account_state_blob,
            AccountStateProof::new(txn_info_with_proof, sparse_merkle_proof),
        ))
    }

    fn get_startup_info(&self) -> Result<Option<StartupInfo>> {
        self.ledger_store.get_startup_info()
    }

    fn get_account_state_with_proof_by_version(
        &self,
        address: AccountAddress,
        version: Version,
    ) -> Result<(Option<AccountStateBlob>, SparseMerkleProof)> {
        self.state_store
            .get_account_state_with_proof_by_version(address, version)
    }

    fn get_latest_state_root(&self) -> Result<(Version, HashValue)> {
        let (version, txn_info) = self.ledger_store.get_latest_transaction_info()?;
        Ok((version, txn_info.state_root_hash()))
    }

    fn get_latest_tree_state(&self) -> Result<TreeState> {
        let tree_state = match self.ledger_store.get_latest_transaction_info_option()? {
            Some((version, txn_info)) => self.ledger_store.get_tree_state(version + 1, txn_info)?,
            None => TreeState::new(
                0,
                vec![],
                self.state_store
                    .get_root_hash_option(PRE_GENESIS_VERSION)?
                    .unwrap_or(*SPARSE_MERKLE_PLACEHOLDER_HASH),
            ),
        };

        Ok(tree_state)
    }

    fn get_block_timestamp(&self, version: u64) -> Result<u64> {
        let ts = match self.transaction_store.get_block_metadata(version)? {
            Some((_v, block_meta)) => block_meta.into_inner()?.1,
            // genesis timestamp is 0
            None => 0,
        };
        Ok(ts)
    }
}

impl DbWriter for LibraDB {
    /// `first_version` is the version of the first transaction in `txns_to_commit`.
    /// When `ledger_info_with_sigs` is provided, verify that the transaction accumulator root hash
    /// it carries is generated after the `txns_to_commit` are applied.
    /// Note that even if `txns_to_commit` is empty, `frist_version` is checked to be
    /// `ledger_info_with_sigs.ledger_info.version + 1` if `ledger_info_with_sigs` is not `None`.
    fn save_transactions(
        &self,
        txns_to_commit: &[TransactionToCommit],
        first_version: Version,
        ledger_info_with_sigs: Option<&LedgerInfoWithSignatures>,
    ) -> Result<()> {
        let num_txns = txns_to_commit.len() as u64;
        // ledger_info_with_sigs could be None if we are doing state synchronization. In this case
        // txns_to_commit should not be empty. Otherwise it is okay to commit empty blocks.
        ensure!(
            ledger_info_with_sigs.is_some() || num_txns > 0,
            "txns_to_commit is empty while ledger_info_with_sigs is None.",
        );

        if let Some(x) = ledger_info_with_sigs {
            let claimed_last_version = x.ledger_info().version();
            ensure!(
                claimed_last_version + 1 == first_version + num_txns,
                "Transaction batch not applicable: first_version {}, num_txns {}, last_version {}",
                first_version,
                num_txns,
                claimed_last_version,
            );
        }

        // Gather db mutations to `batch`.
        let mut cs = ChangeSet::new();

        let new_root_hash = self.save_transactions_impl(txns_to_commit, first_version, &mut cs)?;

        // If expected ledger info is provided, verify result root hash and save the ledger info.
        if let Some(x) = ledger_info_with_sigs {
            let expected_root_hash = x.ledger_info().transaction_accumulator_hash();
            ensure!(
                new_root_hash == expected_root_hash,
                "Root hash calculated doesn't match expected. {:?} vs {:?}",
                new_root_hash,
                expected_root_hash,
            );

            self.ledger_store.put_ledger_info(x, &mut cs)?;
        }

        // Persist.
        let (sealed_cs, counters) = self.seal_change_set(first_version, num_txns, cs)?;
        self.commit(sealed_cs)?;
        // Once everything is successfully persisted, update the latest in-memory ledger info.
        if let Some(x) = ledger_info_with_sigs {
            self.ledger_store.set_latest_ledger_info(x.clone());
        }

        // Only increment counter if commit succeeds and there are at least one transaction written
        // to the storage. That's also when we'd inform the pruner thread to work.
        if num_txns > 0 {
            let last_version = first_version + num_txns - 1;
            OP_COUNTER.inc_by("committed_txns", num_txns as usize);
            LIBRA_STORAGE_COMMITTED_TXNS.inc_by(num_txns as i64);
            OP_COUNTER.set("latest_transaction_version", last_version as usize);
            LIBRA_STORAGE_LATEST_TXN_VERSION.set(last_version as i64);
            counters
                .expect("Counters should be bumped with transactions being saved.")
                .bump_op_counters();

            self.wake_pruner(last_version);
        }

        Ok(())
    }
}

// Convert requested range and order to a range in ascending order.
fn get_first_seq_num_and_limit(ascending: bool, cursor: u64, limit: u64) -> Result<(u64, u64)> {
    ensure!(limit > 0, "limit should > 0, got {}", limit);

    Ok(if ascending {
        (cursor, limit)
    } else if limit <= cursor {
        (cursor - limit + 1, limit)
    } else {
        (0, cursor + 1)
    })
}
