// Portions Copyright (c) Mysten Labs, Inc.
// Copyright (c) Inodra (modifications)
// SPDX-License-Identifier: Apache-2.0
//
// The executor-call sequence and input-object resolution here follow the
// structure of Mysten Labs' `sui-replay-2` crate (Apache-2.0), generalized from
// replaying a known historical transaction to executing a *new* transaction
// against forked state. See the NOTICE file for attribution.

//! The fork virtual machine.
//!
//! [`Vm`] owns the Sui `Executor` for the fork's protocol version and runs a
//! [`TransactionData`] against a data store, returning the effects and the set
//! of objects the caller should commit to the overlay.

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use sui_data_store::{ObjectKey, ObjectStore as DataObjectStore, VersionQuery};
use sui_execution::Executor;
use sui_types::base_types::ObjectID;
use sui_types::digests::TransactionDigest;
use sui_types::effects::{TransactionEffects, TransactionEffectsAPI};
use sui_types::error::ExecutionError;
use sui_types::execution_params::{
    ExecutionOrEarlyError, FundsWithdrawStatus, get_early_execution_error,
};
use sui_types::gas::SuiGasStatus;
use sui_types::metrics::ExecutionMetrics;
use sui_types::object::Object;
use sui_types::supported_protocol_versions::ProtocolConfig;
use sui_types::transaction::{
    CheckedInputObjects, InputObjectKind, InputObjects, ObjectReadResult, ObjectReadResultKind,
    TransactionData, TransactionDataAPI,
};

use crate::engine::runtime_store::RuntimeStore;

/// What a single execution produced, plus the overlay mutations to commit.
pub struct ExecutionOutcome {
    /// The transaction's digest (derived from its data).
    pub digest: TransactionDigest,
    /// The transaction that was executed.
    pub tx_data: TransactionData,
    /// `Ok(())` if the transaction executed successfully, else the Move/VM error.
    pub status: Result<(), ExecutionError>,
    /// Effects of the execution.
    pub effects: TransactionEffects,
    /// Final gas status.
    pub gas_status: SuiGasStatus,
    /// Objects created or mutated — to be written into the overlay.
    pub written: Vec<Object>,
    /// Object ids removed (deleted/wrapped) — to be tombstoned.
    pub deleted: Vec<ObjectID>,
}

/// The fork VM, pinned to one protocol version / epoch.
pub struct Vm {
    executor: Arc<dyn Executor + Send + Sync>,
    execution_metrics: Arc<ExecutionMetrics>,
    protocol_config: ProtocolConfig,
    epoch: u64,
    epoch_start_timestamp_ms: u64,
    reference_gas_price: u64,
}

impl Vm {
    /// Build a VM for `protocol_config` at `epoch`, using the fork point's epoch
    /// timestamp and reference gas price.
    pub fn new(
        protocol_config: ProtocolConfig,
        epoch: u64,
        epoch_start_timestamp_ms: u64,
        reference_gas_price: u64,
    ) -> Result<Self> {
        let executor = sui_execution::executor(&protocol_config, /* silent */ true)
            .map_err(|e| anyhow!("failed to construct executor: {e}"))?;
        let registry = prometheus::Registry::new();
        let execution_metrics = Arc::new(ExecutionMetrics::new(&registry));
        Ok(Self {
            executor,
            execution_metrics,
            protocol_config,
            epoch,
            epoch_start_timestamp_ms,
            reference_gas_price,
        })
    }

    /// The protocol config this VM executes under.
    pub fn protocol_config(&self) -> &ProtocolConfig {
        &self.protocol_config
    }

    /// The reference gas price of the fork's epoch (a valid `gas_price` floor).
    pub fn reference_gas_price(&self) -> u64 {
        self.reference_gas_price
    }

    /// The fork's epoch.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Execute `tx_data` against `store` (reads pinned at `fork_checkpoint`).
    ///
    /// This does **not** mutate the store; the caller commits
    /// [`ExecutionOutcome::written`] / [`ExecutionOutcome::deleted`] on success.
    pub fn execute<S: DataObjectStore>(
        &self,
        store: &S,
        fork_checkpoint: u64,
        tx_data: TransactionData,
    ) -> Result<ExecutionOutcome> {
        let digest = tx_data.digest();
        let input_objects = resolve_input_objects(store, fork_checkpoint, &tx_data)?;
        let checked = CheckedInputObjects::new_for_replay(input_objects);

        let gas_status = if tx_data.kind().is_system_tx() {
            SuiGasStatus::new_unmetered()
        } else {
            SuiGasStatus::new(
                tx_data.gas_data().budget,
                tx_data.gas_data().price,
                self.reference_gas_price,
                &self.protocol_config,
            )
            .map_err(|e| anyhow!("invalid gas parameters: {e}"))?
        };

        let deny_set: HashSet<TransactionDigest> = HashSet::new();
        let early_error = get_early_execution_error(
            &digest,
            &checked,
            &deny_set,
            &FundsWithdrawStatus::MaybeSufficient,
        );
        // No accumulator (settlement) version: that gate only applies to
        // mainnet committed execution, not a local fork.
        let execution_params = match early_error {
            None => ExecutionOrEarlyError::ok(None),
            Some(errors) => ExecutionOrEarlyError::failed(errors, None),
        };

        let runtime_store = RuntimeStore::new(store, fork_checkpoint);
        let mut trace = None;
        let (inner_store, gas_status, effects, _timing, status) = self
            .executor
            .execute_transaction_to_effects_and_execution_error(
                &runtime_store,
                &self.protocol_config,
                self.execution_metrics.clone(),
                /* enable_expensive_checks */ false,
                execution_params,
                &self.epoch,
                self.epoch_start_timestamp_ms,
                checked,
                tx_data.gas_data().clone(),
                gas_status,
                tx_data.kind().clone(),
                /* rewritten_inputs */ None,
                tx_data.sender(),
                digest,
                &mut trace,
            );

        let written: Vec<Object> = inner_store.written.values().cloned().collect();
        let mut deleted: Vec<ObjectID> = Vec::new();
        deleted.extend(effects.deleted().into_iter().map(|r| r.0));
        deleted.extend(effects.wrapped().into_iter().map(|r| r.0));
        deleted.extend(effects.unwrapped_then_deleted().into_iter().map(|r| r.0));

        Ok(ExecutionOutcome {
            digest,
            tx_data,
            status,
            effects,
            gas_status,
            written,
            deleted,
        })
    }
}

/// Resolve the objects a transaction reads from the store, mirroring the kinds
/// the runtime expects: packages and shared objects at the fork checkpoint,
/// owned objects at the exact version the transaction pins.
fn resolve_input_objects<S: DataObjectStore>(
    store: &S,
    fork_checkpoint: u64,
    tx_data: &TransactionData,
) -> Result<InputObjects> {
    let kinds = tx_data
        .input_objects()
        .map_err(|e| anyhow!("could not enumerate input objects: {e}"))?;
    let mut resolved = Vec::with_capacity(kinds.len());

    for kind in &kinds {
        match kind {
            InputObjectKind::MovePackage(package_id) => {
                let object = fetch_one(
                    store,
                    *package_id,
                    VersionQuery::AtCheckpoint(fork_checkpoint),
                )?
                .ok_or_else(|| anyhow!("package {package_id} not found at fork checkpoint"))?;
                resolved.push(ObjectReadResult {
                    input_object_kind: *kind,
                    object: ObjectReadResultKind::Object(object),
                });
            }
            InputObjectKind::ImmOrOwnedMoveObject((object_id, version, _digest)) => {
                let object = fetch_one(store, *object_id, VersionQuery::Version(version.value()))?
                    .ok_or_else(|| {
                        anyhow!("owned object {object_id} v{} not found", version.value())
                    })?;
                // Re-key to the object's actual reference (id, version, digest).
                let input_object_kind =
                    InputObjectKind::ImmOrOwnedMoveObject(object.compute_object_reference());
                resolved.push(ObjectReadResult {
                    input_object_kind,
                    object: ObjectReadResultKind::Object(object),
                });
            }
            InputObjectKind::SharedMoveObject {
                id,
                initial_shared_version,
                mutability,
            } => {
                // Consensus assigns shared-object versions; in a fork we take the
                // current version as of the fork checkpoint.
                let object = fetch_one(store, *id, VersionQuery::AtCheckpoint(fork_checkpoint))?
                    .ok_or_else(|| anyhow!("shared object {id} not found at fork checkpoint"))?;
                let input_object_kind = InputObjectKind::SharedMoveObject {
                    id: *id,
                    initial_shared_version: *initial_shared_version,
                    mutability: *mutability,
                };
                resolved.push(ObjectReadResult {
                    input_object_kind,
                    object: ObjectReadResultKind::Object(object),
                });
            }
        }
    }

    Ok(InputObjects::new(resolved))
}

fn fetch_one<S: DataObjectStore>(
    store: &S,
    id: ObjectID,
    version_query: VersionQuery,
) -> Result<Option<Object>> {
    Ok(store
        .get_objects(&[ObjectKey {
            object_id: id,
            version_query,
        }])
        .map_err(|e| anyhow!("store read failed for {id}: {e}"))?
        .into_iter()
        .next()
        .flatten()
        .map(|(object, _version)| object))
}
