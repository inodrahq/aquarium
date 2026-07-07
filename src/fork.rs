// Copyright (c) Inodra
// SPDX-License-Identifier: Apache-2.0

//! The top-level fork handle.
//!
//! A [`Fork`] pins a checkpoint of mainnet, wraps the backing read store in an
//! [`OverlayStore`], and exposes reads (which fall through to mainnet) plus —
//! under the `execute` feature — transaction execution that mutates the overlay.

use anyhow::{Context, Result};
use sui_data_store::node::Node;
use sui_data_store::stores::DataStore;
use sui_data_store::{ObjectKey, ObjectStore, VersionQuery};
use sui_types::base_types::ObjectID;
use sui_types::object::Object;

use crate::store::OverlayStore;

/// A fork of mainnet (GraphQL-backed) pinned at a checkpoint.
pub type MainnetFork = Fork<DataStore>;

/// A local fork over a backing read store `S`, pinned at `fork_checkpoint`.
pub struct Fork<S> {
    store: OverlayStore<S>,
    fork_checkpoint: u64,
    /// Serializes transaction execution: a fork is a serial sequencer, so
    /// concurrent `execute` calls run one at a time (no lost updates / TOCTOU on
    /// object versions). Reads are unaffected.
    #[cfg(feature = "execute")]
    execution_lock: std::sync::Mutex<()>,
}

impl Fork<DataStore> {
    /// Create a fork of mainnet at `fork_checkpoint`, reading through Mysten's
    /// public GraphQL endpoint.
    pub fn mainnet(fork_checkpoint: u64) -> Result<Self> {
        let data_store = DataStore::new(Node::Mainnet, env!("CARGO_PKG_VERSION"))
            .context("constructing mainnet GraphQL data store")?;
        Ok(Self::with_store(data_store, fork_checkpoint))
    }
}

impl<S> Fork<S> {
    /// Build a fork over an arbitrary backing read store.
    pub fn with_store(inner: S, fork_checkpoint: u64) -> Self {
        Self {
            store: OverlayStore::new(inner, fork_checkpoint),
            fork_checkpoint,
            #[cfg(feature = "execute")]
            execution_lock: std::sync::Mutex::new(()),
        }
    }

    /// The checkpoint this fork branched from.
    pub fn fork_checkpoint(&self) -> u64 {
        self.fork_checkpoint
    }

    /// Borrow the overlay store (engine and tests reach in here).
    pub fn store(&self) -> &OverlayStore<S> {
        &self.store
    }
}

impl<S> Fork<S>
where
    S: ObjectStore,
{
    /// Read an object as the fork currently sees it: overlay first, otherwise
    /// mainnet at the fork checkpoint.
    pub fn object(&self, id: ObjectID) -> Result<Option<Object>> {
        let key = ObjectKey {
            object_id: id,
            version_query: VersionQuery::AtCheckpoint(self.fork_checkpoint),
        };
        Ok(self
            .store
            .get_objects(&[key])?
            .into_iter()
            .next()
            .flatten()
            .map(|(o, _)| o))
    }

    /// Read a specific historical version of an object (always from mainnet
    /// unless that exact version happens to be the current overlay version).
    pub fn object_at_version(&self, id: ObjectID, version: u64) -> Result<Option<Object>> {
        let key = ObjectKey {
            object_id: id,
            version_query: VersionQuery::Version(version),
        };
        Ok(self
            .store
            .get_objects(&[key])?
            .into_iter()
            .next()
            .flatten()
            .map(|(o, _)| o))
    }
}

#[cfg(feature = "execute")]
impl<S> Fork<S>
where
    S: ObjectStore + sui_data_store::EpochStore,
{
    /// Build a [`Vm`](crate::engine::Vm) for executing transactions against this
    /// fork, using `epoch`'s protocol config / timestamp / reference gas price.
    ///
    /// `epoch` **must** be the epoch that the fork checkpoint belongs to —
    /// passing a different epoch silently yields the wrong protocol config and
    /// gas/timestamp parameters. Prefer [`MainnetFork::vm`], which derives the
    /// epoch from the fork checkpoint for you.
    pub fn vm_for_epoch(&self, epoch: u64) -> Result<crate::engine::Vm> {
        use sui_data_store::EpochStore;
        let epoch_data = self
            .store
            .epoch_info(epoch)?
            .ok_or_else(|| anyhow::anyhow!("epoch {epoch} not found in data store"))?;
        let protocol_config = self
            .store
            .protocol_config(epoch)?
            .ok_or_else(|| anyhow::anyhow!("protocol config for epoch {epoch} not found"))?;
        crate::engine::Vm::new(
            protocol_config,
            epoch,
            epoch_data.start_timestamp,
            epoch_data.rgp,
        )
    }

    /// Execute a transaction against the fork without committing — a dry run.
    /// The overlay is left unchanged; useful for gas estimation or previewing
    /// effects.
    pub fn simulate(
        &self,
        vm: &crate::engine::Vm,
        tx_data: sui_types::transaction::TransactionData,
    ) -> Result<crate::engine::ExecutionOutcome> {
        vm.execute(&self.store, self.fork_checkpoint, tx_data)
    }

    /// Execute a transaction against the fork and commit its effects to the
    /// overlay; mainnet is untouched.
    ///
    /// The effects are committed whether or not the Move execution *succeeded*:
    /// a transaction that aborts still produces well-formed effects (gas is
    /// charged and the gas object is bumped), exactly as on the live chain. The
    /// returned [`ExecutionOutcome::status`] reports the Move-level result.
    ///
    /// Calls are serialized per fork: input resolution, execution and commit run
    /// under one lock, so concurrent `execute` calls do not interleave — each
    /// sees the previous one's committed effects.
    ///
    /// Like Sui transaction replay, input objects are taken at the versions the
    /// transaction pins (validation is bypassed). The caller is responsible for
    /// building each transaction against current object versions — e.g. re-read
    /// owned objects from the fork after a previous `execute` (as the `demo`
    /// command does for the gas coin).
    pub fn execute(
        &self,
        vm: &crate::engine::Vm,
        tx_data: sui_types::transaction::TransactionData,
    ) -> Result<crate::engine::ExecutionOutcome> {
        // Recover from a poisoned lock rather than propagating the panic: a
        // previous execution that panicked (e.g. deep in the Move VM on a
        // malformed input) must not brick every subsequent transaction. The
        // guarded data is just a serialization token, so the poisoned inner
        // value is safe to reuse.
        let _serialize = self
            .execution_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let outcome = vm.execute(&self.store, self.fork_checkpoint, tx_data)?;
        // Apply writes, deletions, address-balance (accumulator) deposits and
        // the transaction record atomically, so a concurrent reader never sees
        // a half-committed transaction.
        use sui_types::effects::TransactionEffectsAPI;
        self.store.commit(
            &outcome.written,
            &outcome.deleted,
            &outcome.effects.accumulator_events(),
            outcome.digest.to_string(),
            sui_data_store::TransactionInfo {
                data: outcome.tx_data.clone(),
                effects: outcome.effects.clone(),
                checkpoint: self.fork_checkpoint,
            },
        );
        Ok(outcome)
    }
}

#[cfg(feature = "execute")]
impl Fork<DataStore> {
    /// Build a [`Vm`](crate::engine::Vm) for this mainnet fork, deriving the
    /// epoch from the fork checkpoint (so the correct protocol config, epoch
    /// timestamp and reference gas price are always used).
    pub fn vm(&self) -> Result<crate::engine::Vm> {
        let epoch = crate::gql::Gql::mainnet()?.checkpoint_epoch(self.fork_checkpoint)?;
        self.vm_for_epoch(epoch)
    }
}
