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
///
/// `epoch` and `epoch_start_timestamp_ms` are interior-mutable (atomics): a
/// fork can *shallow-advance* the epoch (see [`Vm::advance_epoch`]) so that
/// `tx_context::epoch()` / `epoch_timestamp_ms()` report a later epoch, which
/// unblocks epoch-gated logic (e.g. maturing a staking withdrawal) without a
/// real end-of-epoch settlement. The `protocol_config`/`executor` stay fixed —
/// advancing within one protocol version reuses the same executor.
pub struct Vm {
    executor: Arc<dyn Executor + Send + Sync>,
    execution_metrics: Arc<ExecutionMetrics>,
    protocol_config: ProtocolConfig,
    epoch: std::sync::atomic::AtomicU64,
    epoch_start_timestamp_ms: std::sync::atomic::AtomicU64,
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
            epoch: std::sync::atomic::AtomicU64::new(epoch),
            epoch_start_timestamp_ms: std::sync::atomic::AtomicU64::new(epoch_start_timestamp_ms),
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

    /// The fork's current epoch (may have been shallow-advanced).
    pub fn epoch(&self) -> u64 {
        self.epoch.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The fork's current epoch-start timestamp (ms).
    pub fn epoch_start_timestamp_ms(&self) -> u64 {
        self.epoch_start_timestamp_ms
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Shallow-advance the epoch by `count` epochs, moving the epoch-start
    /// timestamp to `new_start_timestamp_ms`. Subsequent transactions see the
    /// bumped epoch via `TxContext`. This does **not** run a real end-of-epoch
    /// change (no staking-reward distribution, no validator rotation, and the
    /// on-chain `SuiSystemState` at `0x5` is left untouched) — it only crosses
    /// the epoch boundary as the VM presents it, which is what epoch-gated Move
    /// checks read. Returns the new epoch.
    pub fn advance_epoch(&self, count: u64, new_start_timestamp_ms: u64) -> u64 {
        self.epoch_start_timestamp_ms
            .store(new_start_timestamp_ms, std::sync::atomic::Ordering::Relaxed);
        self.epoch
            .fetch_add(count.max(1), std::sync::atomic::Ordering::Relaxed)
            + count.max(1)
    }

    /// Restore the epoch and epoch-start timestamp to exact values — used when
    /// reverting a fork snapshot, so `TxContext` epoch/time roll back with the
    /// overlay.
    pub fn restore_epoch(&self, epoch: u64, epoch_start_timestamp_ms: u64) {
        self.epoch
            .store(epoch, std::sync::atomic::Ordering::Relaxed);
        self.epoch_start_timestamp_ms.store(
            epoch_start_timestamp_ms,
            std::sync::atomic::Ordering::Relaxed,
        );
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
            // The fork executes replay-style, skipping mainnet's pre-execution
            // validity checks. Re-apply the gas-payment check here: otherwise a
            // budget above the gas coins' balance underflows the gas charger and
            // panics mid-execution instead of failing cleanly.
            check_gas_payment(store, &tx_data, self.reference_gas_price)?;
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

        // Snapshot the (interior-mutable) epoch parameters for this execution;
        // the executor takes `&epoch`.
        let epoch = self.epoch();
        let epoch_start_timestamp_ms = self.epoch_start_timestamp_ms();

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
                &epoch,
                epoch_start_timestamp_ms,
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

/// Replicate the parts of Sui's pre-execution gas check that the fork skips
/// (it runs unvalidated, replay-style). Returns errors prefixed `InsufficientGas`
/// / `InvalidGasPrice` so callers can surface a precise status instead of the
/// gas charger panicking on an underflow.
fn check_gas_payment<S: DataObjectStore>(
    store: &S,
    tx_data: &TransactionData,
    reference_gas_price: u64,
) -> Result<()> {
    use sui_types::gas_coin::GasCoin;

    let gas = tx_data.gas_data();
    // An empty payment vector means the transaction pays gas from the sender's
    // address balance (gasless / v2 address-balance gas), not from coins — there
    // are no coins to sum, so leave that path to the executor.
    if gas.payment.is_empty() {
        return Ok(());
    }
    if gas.price < reference_gas_price {
        anyhow::bail!(
            "InvalidGasPrice: gas price {} is below the reference gas price {reference_gas_price}",
            gas.price
        );
    }
    let mut balance: u128 = 0;
    for (id, version, _digest) in &gas.payment {
        let object = fetch_one(store, *id, VersionQuery::Version(version.value()))?
            .ok_or_else(|| anyhow!("gas coin {id} v{} not found", version.value()))?;
        let coin = GasCoin::try_from(&object)
            .map_err(|e| anyhow!("gas payment object {id} is not a SUI coin: {e}"))?;
        balance += coin.value() as u128;
    }
    if balance < gas.budget as u128 {
        anyhow::bail!(
            "InsufficientGas: gas payment balance {balance} MIST is below the gas budget {} MIST",
            gas.budget
        );
    }
    Ok(())
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
                // A tx may only pass an immutable or address-owned object as an
                // `ImmOrOwnedMoveObject`. Passing an object owned by another
                // object (a dynamic-field child) or a shared object here makes
                // the executor panic ("Unexpected owner"); catch it as a clean
                // error, as mainnet's input validation would.
                use sui_types::object::Owner;
                match object.owner() {
                    Owner::AddressOwner(_) | Owner::Immutable => {}
                    other => anyhow::bail!(
                        "InvalidInput: object {object_id} is passed as an owned input but its \
                         owner is {other:?} (only address-owned or immutable objects may be used \
                         this way)"
                    ),
                }
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
