// Copyright (c) Inodra
// SPDX-License-Identifier: Apache-2.0

//! The writable fork overlay.
//!
//! [`OverlayStore`] wraps any Mysten `sui_data_store` read store (typically a
//! GraphQL-backed [`sui_data_store::stores::DataStore`] pinned at the fork
//! checkpoint) and layers a local, in-memory set of objects produced by
//! transactions executed inside the fork.
//!
//! Reads resolve **overlay first, then mainnet**:
//!   1. an object created/mutated locally is served from the overlay,
//!   2. an object deleted locally is reported absent (for "current" queries),
//!   3. otherwise the read falls through to the backing mainnet store.
//!
//! All local state lives behind a single lock, so [`OverlayStore::commit`]
//! applies a transaction's writes, deletions and record atomically — a
//! concurrent reader never observes a half-applied transaction. The real chain
//! is never touched.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::RwLock;

use anyhow::Error;
use sui_data_store::{
    EpochData, EpochStore, ObjectKey, ObjectStore, TransactionInfo, TransactionStore, VersionQuery,
};
use sui_types::base_types::ObjectID;
use sui_types::object::Object;
use sui_types::supported_protocol_versions::ProtocolConfig;

/// Local fork state, guarded as a unit so commits are atomic.
///
/// `Clone` is what makes fork snapshots cheap: capturing a snapshot is a clone
/// of this whole struct, and reverting swaps a clone back in (see
/// [`OverlayStore::snapshot`] / [`OverlayStore::restore`]).
#[derive(Clone, Default)]
pub(crate) struct OverlayState {
    /// Objects created or mutated by locally executed transactions, keyed by id
    /// and holding the *latest* local version.
    objects: BTreeMap<ObjectID, Object>,
    /// Objects deleted by locally executed transactions.
    tombstones: BTreeSet<ObjectID>,
    /// Transactions executed locally, by digest (Base58).
    transactions: BTreeMap<String, TransactionInfo>,
    /// Digests of locally executed transactions in execution order (oldest
    /// first), so the fork can present them as a synthetic checkpoint feed.
    executed_order: Vec<String>,
    /// Address balances (accumulator deposits) produced by local transactions,
    /// keyed by `(owner, canonical coin type)`. On the live chain these writes
    /// settle into accumulator objects via system transactions; a fork has no
    /// settlement, so the net amounts are tracked here directly.
    address_balances: BTreeMap<(sui_types::base_types::SuiAddress, String), u128>,
}

/// The overlay in a form that serializes to disk (see
/// [`OverlayStore::export`] / [`OverlayStore::import`]). [`TransactionInfo`] is
/// not `Serialize`, so its public parts are stored directly.
#[cfg(feature = "serve")]
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct PersistedOverlay {
    objects: Vec<Object>,
    tombstones: Vec<ObjectID>,
    transactions: Vec<(
        String,
        sui_types::transaction::TransactionData,
        sui_types::effects::TransactionEffects,
        u64,
    )>,
    executed_order: Vec<String>,
    address_balances: Vec<(sui_types::base_types::SuiAddress, String, u128)>,
}

/// A writable fork overlay on top of a read-only mainnet data store `S`.
pub struct OverlayStore<S> {
    /// Backing read store (mainnet), pinned at the fork checkpoint by callers.
    inner: S,
    /// The checkpoint the fork was created from. All fall-through "current"
    /// reads of un-mutated objects are anchored here.
    fork_checkpoint: u64,
    /// All locally mutated state, behind one lock for atomic commits.
    state: RwLock<OverlayState>,
}

impl<S> OverlayStore<S> {
    /// Wrap a backing read store to create an (empty) fork at `fork_checkpoint`.
    pub fn new(inner: S, fork_checkpoint: u64) -> Self {
        Self {
            inner,
            fork_checkpoint,
            state: RwLock::new(OverlayState::default()),
        }
    }

    /// The checkpoint this fork branched from.
    pub fn fork_checkpoint(&self) -> u64 {
        self.fork_checkpoint
    }

    /// Borrow the backing mainnet store.
    pub fn inner(&self) -> &S {
        &self.inner
    }

    /// Number of objects currently held in the local overlay.
    pub fn overlay_len(&self) -> usize {
        self.state
            .read()
            .expect("overlay state poisoned")
            .objects
            .len()
    }

    /// Atomically apply a transaction's effects to the overlay: written objects
    /// are inserted (clearing any tombstone), deleted ids are tombstoned,
    /// accumulator (address-balance) writes are netted, and the transaction is
    /// recorded — all under one lock.
    ///
    /// Crate-internal: the supported way to mutate a fork is [`Fork::execute`],
    /// which serializes commits. Direct callers would bypass that serialization.
    #[cfg(feature = "execute")]
    pub(crate) fn commit(
        &self,
        written: &[Object],
        deleted: &[ObjectID],
        accumulator_events: &[sui_types::accumulator_event::AccumulatorEvent],
        digest: String,
        info: TransactionInfo,
    ) {
        let mut state = self.state.write().expect("overlay state poisoned");
        for object in written {
            let id = object.id();
            state.tombstones.remove(&id);
            state.objects.insert(id, object.clone());
        }
        for id in deleted {
            state.objects.remove(id);
            state.tombstones.insert(*id);
        }
        for event in accumulator_events {
            apply_accumulator_event(&mut state.address_balances, event);
        }
        if state.transactions.insert(digest.clone(), info).is_none() {
            state.executed_order.push(digest);
        }
    }

    /// Capture the entire local overlay as an independent snapshot (a deep
    /// clone). Later mutations do not affect it, so it can be handed to
    /// [`OverlayStore::restore`] any number of times to roll back to this point
    /// (the anvil `evm_snapshot` analog).
    #[cfg(feature = "execute")]
    pub(crate) fn snapshot(&self) -> OverlayState {
        self.state.read().expect("overlay state poisoned").clone()
    }

    /// Export the overlay to a serializable form for persistence to disk. The
    /// backing (mainnet) store is not exported — it is re-fetched (through the
    /// cache) on demand, and reads at a pinned checkpoint are immutable.
    #[cfg(feature = "serve")]
    pub(crate) fn export(&self) -> PersistedOverlay {
        let state = self.state.read().expect("overlay state poisoned");
        PersistedOverlay {
            objects: state.objects.values().cloned().collect(),
            tombstones: state.tombstones.iter().copied().collect(),
            transactions: state
                .transactions
                .iter()
                .map(|(digest, info)| {
                    (
                        digest.clone(),
                        info.data.clone(),
                        info.effects.clone(),
                        info.checkpoint,
                    )
                })
                .collect(),
            executed_order: state.executed_order.clone(),
            address_balances: state
                .address_balances
                .iter()
                .map(|((owner, ty), amount)| (*owner, ty.clone(), *amount))
                .collect(),
        }
    }

    /// Clear the entire local overlay back to empty — every locally executed
    /// transaction, object write, tombstone and address balance is dropped (the
    /// anvil `reset` analog). The backing network state is untouched.
    #[cfg(feature = "execute")]
    pub(crate) fn clear(&self) {
        *self.state.write().expect("overlay state poisoned") = OverlayState::default();
    }

    /// Replace the overlay with a previously [`export`](Self::export)ed one.
    #[cfg(feature = "serve")]
    pub(crate) fn import(&self, persisted: PersistedOverlay) {
        let mut state = self.state.write().expect("overlay state poisoned");
        state.objects = persisted.objects.into_iter().map(|o| (o.id(), o)).collect();
        state.tombstones = persisted.tombstones.into_iter().collect();
        state.transactions = persisted
            .transactions
            .into_iter()
            .map(|(digest, data, effects, checkpoint)| {
                (
                    digest,
                    TransactionInfo {
                        data,
                        effects,
                        checkpoint,
                    },
                )
            })
            .collect();
        state.executed_order = persisted.executed_order;
        state.address_balances = persisted
            .address_balances
            .into_iter()
            .map(|(owner, ty, amount)| ((owner, ty), amount))
            .collect();
    }

    /// Replace the local overlay with a previously captured [`snapshot`], rolling
    /// back every object write, deletion, transaction record and address balance
    /// to that point (`evm_revert`). The backing mainnet store is unaffected.
    ///
    /// [`snapshot`]: OverlayStore::snapshot
    #[cfg(feature = "execute")]
    pub(crate) fn restore(&self, snapshot: OverlayState) {
        *self.state.write().expect("overlay state poisoned") = snapshot;
    }

    /// Force an object into the overlay directly, outside transaction execution
    /// — the primitive behind the fork's "cheat" controls (advance the clock,
    /// override an oracle's price object, etc., analogous to anvil's
    /// `setStorageAt`). The caller is responsible for handing over a well-formed
    /// object at a fresh version; any tombstone for the id is cleared.
    #[cfg(feature = "execute")]
    pub(crate) fn set_object(&self, object: Object) {
        let mut state = self.state.write().expect("overlay state poisoned");
        let id = object.id();
        state.tombstones.remove(&id);
        state.objects.insert(id, object);
    }

    /// The most recently executed local transactions (newest first, up to
    /// `limit`), as `(digest, info)` — powers the fork's synthetic checkpoint.
    pub fn recent_executed(&self, limit: usize) -> Vec<(String, TransactionInfo)> {
        let state = self.state.read().expect("overlay state poisoned");
        state
            .executed_order
            .iter()
            .rev()
            .take(limit)
            .filter_map(|d| state.transactions.get(d).map(|i| (d.clone(), i.clone())))
            .collect()
    }

    /// Total number of transactions executed locally against the fork.
    pub fn executed_count(&self) -> usize {
        self.state
            .read()
            .expect("overlay state poisoned")
            .executed_order
            .len()
    }

    /// Commit a single written object (clearing any tombstone). Test helper;
    /// production commits go through [`OverlayStore::commit`].
    #[cfg(test)]
    fn apply_written(&self, object: Object) {
        let mut state = self.state.write().expect("overlay state poisoned");
        let id = object.id();
        state.tombstones.remove(&id);
        state.objects.insert(id, object);
    }

    /// Mark an object deleted. Test helper; production deletes go through
    /// [`OverlayStore::commit`].
    #[cfg(test)]
    fn tombstone(&self, id: ObjectID) {
        let mut state = self.state.write().expect("overlay state poisoned");
        state.objects.remove(&id);
        state.tombstones.insert(id);
    }

    /// Snapshot of the current local version of `id`, if present in the overlay.
    pub fn overlay_object(&self, id: &ObjectID) -> Option<Object> {
        self.state
            .read()
            .expect("overlay state poisoned")
            .objects
            .get(id)
            .cloned()
    }

    /// Snapshot of every object currently held in the local overlay.
    pub fn overlay_objects(&self) -> Vec<Object> {
        self.state
            .read()
            .expect("overlay state poisoned")
            .objects
            .values()
            .cloned()
            .collect()
    }

    /// Ids the overlay has an opinion about (locally written or deleted): reads
    /// of these must not fall through to mainnet for "current" state.
    pub fn overlay_touched_ids(&self) -> BTreeSet<ObjectID> {
        let state = self.state.read().expect("overlay state poisoned");
        state
            .objects
            .keys()
            .chain(state.tombstones.iter())
            .copied()
            .collect()
    }

    /// Every `(canonical coin type, net amount)` address balance the overlay has
    /// accrued for `owner` (locally deposited via accumulator writes). Powers
    /// `ListBalances`' address-balance contribution.
    pub fn address_balances_of(
        &self,
        owner: sui_types::base_types::SuiAddress,
    ) -> Vec<(String, u128)> {
        self.state
            .read()
            .expect("overlay state poisoned")
            .address_balances
            .iter()
            .filter(|((o, _), _)| *o == owner)
            .map(|((_, ty), amount)| (ty.clone(), *amount))
            .collect()
    }

    /// Net address balance (accumulator deposits minus withdrawals) produced by
    /// locally executed transactions for `(owner, coin type)`. `coin_type` is
    /// the coin's canonical type string, e.g. the output of
    /// `TypeTag::to_canonical_string(true)` for `0x2::sui::SUI`.
    ///
    /// A fork starts with mainnet's settled address balances unreadable through
    /// object queries (they live in accumulator child objects); this tracks the
    /// *local* delta, which for fresh fork-only accounts is the whole balance.
    pub fn address_balance(
        &self,
        owner: sui_types::base_types::SuiAddress,
        coin_type: &str,
    ) -> u128 {
        self.state
            .read()
            .expect("overlay state poisoned")
            .address_balances
            .get(&(owner, coin_type.to_string()))
            .copied()
            .unwrap_or(0)
    }
}

/// Net an accumulator write into the local address-balance map. Only
/// `Balance<T>` integer accumulators are represented (the only kind
/// `coin::send_funds` / address-balance transfers produce today); other
/// accumulator kinds (e.g. event-stream digests) are ignored.
#[cfg(feature = "execute")]
fn apply_accumulator_event(
    balances: &mut BTreeMap<(sui_types::base_types::SuiAddress, String), u128>,
    event: &sui_types::accumulator_event::AccumulatorEvent,
) {
    use sui_types::TypeTag;
    use sui_types::effects::{AccumulatorOperation, AccumulatorValue};

    // The accumulator's type is `Balance<T>`; key the map by the inner `T`
    // (the coin type callers query with).
    let TypeTag::Struct(ty) = &event.write.address.ty else {
        return;
    };
    if ty.name.as_str() != "Balance" || ty.type_params.len() != 1 {
        return;
    }
    let coin_type = ty.type_params[0].to_canonical_string(true);
    let AccumulatorValue::Integer(amount) = event.write.value else {
        return;
    };
    let entry = balances
        .entry((event.write.address.address, coin_type))
        .or_insert(0);
    match event.write.operation {
        AccumulatorOperation::Merge => *entry = entry.saturating_add(amount as u128),
        AccumulatorOperation::Split => *entry = entry.saturating_sub(amount as u128),
    }
}

/// Retry a backing-store read a few times on error, with exponential backoff.
///
/// Fork "current" state is read lazily through Mysten's public GraphQL
/// endpoint; under load (e.g. a CLMM swap that walks many tick dynamic fields)
/// it occasionally drops a single request. The read is idempotent and a dropped
/// read, left unhandled, surfaces to the Move VM as a `STORAGE_ERROR` and
/// *panics* the execution — so a bounded retry turns a transient blip into a
/// short delay instead of a failed transaction. Only errors are retried; a
/// genuine "object absent" is `Ok(None)` and returns immediately.
fn with_retry<T>(mut read: impl FnMut() -> Result<T, Error>) -> Result<T, Error> {
    const MAX_ATTEMPTS: u32 = 5;
    let mut attempt: u32 = 0;
    loop {
        match read() {
            Ok(value) => return Ok(value),
            Err(e) => {
                attempt += 1;
                if attempt >= MAX_ATTEMPTS {
                    return Err(e);
                }
                // 150ms, 300ms, 600ms, 1200ms.
                std::thread::sleep(std::time::Duration::from_millis(150 * (1 << (attempt - 1))));
            }
        }
    }
}

/// Does this query ask for the object's *current* state (as opposed to a
/// specific historical version)? Tombstones only suppress current queries; an
/// explicit historical `Version` is still answerable from mainnet.
fn is_current_query(q: &VersionQuery) -> bool {
    matches!(
        q,
        VersionQuery::AtCheckpoint(_) | VersionQuery::RootVersion(_)
    )
}

/// If the overlay object satisfies the query, return it together with its
/// actual version; otherwise `None` (caller falls through to mainnet).
fn overlay_match(obj: &Object, q: &VersionQuery) -> Option<(Object, u64)> {
    let v = obj.version().value();
    match q {
        // Exact version: only the overlay's own (latest) version matches; older
        // versions are historical and live on mainnet.
        VersionQuery::Version(want) => (*want == v).then(|| (obj.clone(), v)),
        // Root version: the overlay object qualifies iff it is at or below the
        // requested upper bound.
        VersionQuery::RootVersion(max) => (v <= *max).then(|| (obj.clone(), v)),
        // "At checkpoint" means current fork state — always the overlay object.
        VersionQuery::AtCheckpoint(_) => Some((obj.clone(), v)),
    }
}

impl<S> ObjectStore for OverlayStore<S>
where
    S: ObjectStore,
{
    fn get_objects(&self, keys: &[ObjectKey]) -> Result<Vec<Option<(Object, u64)>>, Error> {
        let mut out: Vec<Option<(Object, u64)>> = Vec::with_capacity(keys.len());
        let mut miss_slots: Vec<usize> = Vec::new();
        let mut miss_keys: Vec<ObjectKey> = Vec::new();

        {
            let state = self.state.read().expect("overlay state poisoned");
            for key in keys {
                if let Some(obj) = state.objects.get(&key.object_id)
                    && let Some(hit) = overlay_match(obj, &key.version_query)
                {
                    out.push(Some(hit));
                    continue;
                }
                if state.tombstones.contains(&key.object_id) && is_current_query(&key.version_query)
                {
                    out.push(None);
                    continue;
                }
                // Defer to mainnet.
                miss_slots.push(out.len());
                miss_keys.push(key.clone());
                out.push(None);
            }
        }

        if !miss_keys.is_empty() {
            let fetched = with_retry(|| self.inner.get_objects(&miss_keys))?;
            for (slot, value) in miss_slots.into_iter().zip(fetched) {
                out[slot] = value;
            }
        }
        Ok(out)
    }
}

impl<S> EpochStore for OverlayStore<S>
where
    S: EpochStore,
{
    // A fork does not advance epochs: epoch data and protocol config are exactly
    // mainnet's at the fork point, served (and cached) by the backing store.
    fn epoch_info(&self, epoch: u64) -> Result<Option<EpochData>, Error> {
        with_retry(|| self.inner.epoch_info(epoch))
    }

    fn protocol_config(&self, epoch: u64) -> Result<Option<ProtocolConfig>, Error> {
        with_retry(|| self.inner.protocol_config(epoch))
    }
}

impl<S> TransactionStore for OverlayStore<S>
where
    S: TransactionStore,
{
    fn transaction_data_and_effects(
        &self,
        tx_digest: &str,
    ) -> Result<Option<TransactionInfo>, Error> {
        if let Some(info) = self
            .state
            .read()
            .expect("overlay state poisoned")
            .transactions
            .get(tx_digest)
        {
            return Ok(Some(info.clone()));
        }
        with_retry(|| self.inner.transaction_data_and_effects(tx_digest))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use sui_types::base_types::{SequenceNumber, SuiAddress};
    use sui_types::object::Owner;

    #[test]
    fn with_retry_recovers_after_transient_errors() {
        let attempts = Cell::new(0u32);
        let result: Result<u32, Error> = with_retry(|| {
            let n = attempts.get() + 1;
            attempts.set(n);
            if n < 3 {
                Err(anyhow::anyhow!("transient"))
            } else {
                Ok(n)
            }
        });
        assert_eq!(result.unwrap(), 3, "succeeds on the third try");
        assert_eq!(attempts.get(), 3);
    }

    #[test]
    fn with_retry_gives_up_and_returns_last_error() {
        let attempts = Cell::new(0u32);
        let result: Result<u32, Error> = with_retry(|| {
            attempts.set(attempts.get() + 1);
            Err(anyhow::anyhow!("always fails"))
        });
        assert!(result.is_err());
        assert_eq!(attempts.get(), 5, "stops after MAX_ATTEMPTS");
    }

    /// Minimal in-memory backing store standing in for mainnet.
    #[derive(Default)]
    struct MockMainnet {
        /// Exact (id, version) -> object.
        versioned: BTreeMap<(ObjectID, u64), Object>,
        /// Current "latest" version per object id.
        latest: BTreeMap<ObjectID, u64>,
    }

    impl MockMainnet {
        fn insert(&mut self, object: Object) {
            let id = object.id();
            let v = object.version().value();
            self.latest
                .entry(id)
                .and_modify(|cur| *cur = (*cur).max(v))
                .or_insert(v);
            self.versioned.insert((id, v), object);
        }
    }

    impl ObjectStore for MockMainnet {
        fn get_objects(&self, keys: &[ObjectKey]) -> Result<Vec<Option<(Object, u64)>>, Error> {
            Ok(keys
                .iter()
                .map(|k| {
                    let version = match k.version_query {
                        VersionQuery::Version(v) => Some(v),
                        VersionQuery::AtCheckpoint(_) => self.latest.get(&k.object_id).copied(),
                        VersionQuery::RootVersion(max) => {
                            self.latest.get(&k.object_id).copied().filter(|v| *v <= max)
                        }
                    };
                    version.and_then(|v| {
                        self.versioned
                            .get(&(k.object_id, v))
                            .map(|o| (o.clone(), v))
                    })
                })
                .collect())
        }
    }

    impl EpochStore for MockMainnet {
        fn epoch_info(&self, _epoch: u64) -> Result<Option<EpochData>, Error> {
            Ok(None)
        }
        fn protocol_config(&self, _epoch: u64) -> Result<Option<ProtocolConfig>, Error> {
            Ok(None)
        }
    }

    impl TransactionStore for MockMainnet {
        fn transaction_data_and_effects(
            &self,
            _tx_digest: &str,
        ) -> Result<Option<TransactionInfo>, Error> {
            Ok(None)
        }
    }

    fn object(id: ObjectID, version: u64) -> Object {
        Object::with_id_owner_version_for_testing(
            id,
            SequenceNumber::from(version),
            Owner::AddressOwner(SuiAddress::ZERO),
        )
    }

    fn at_checkpoint(id: ObjectID) -> ObjectKey {
        ObjectKey {
            object_id: id,
            version_query: VersionQuery::AtCheckpoint(100),
        }
    }

    fn at_version(id: ObjectID, v: u64) -> ObjectKey {
        ObjectKey {
            object_id: id,
            version_query: VersionQuery::Version(v),
        }
    }

    fn get_one<S: ObjectStore>(store: &S, key: ObjectKey) -> Option<(Object, u64)> {
        store
            .get_objects(&[key])
            .unwrap()
            .into_iter()
            .next()
            .flatten()
    }

    #[test]
    fn falls_through_to_mainnet_when_overlay_empty() {
        let id = ObjectID::random();
        let mut mainnet = MockMainnet::default();
        mainnet.insert(object(id, 5));
        let overlay = OverlayStore::new(mainnet, 100);

        let hit = get_one(&overlay, at_checkpoint(id)).expect("should resolve from mainnet");
        assert_eq!(hit.1, 5);
        assert_eq!(overlay.overlay_len(), 0);
    }

    #[test]
    fn overlay_shadows_mainnet_for_current_reads() {
        let id = ObjectID::random();
        let mut mainnet = MockMainnet::default();
        mainnet.insert(object(id, 5));
        let overlay = OverlayStore::new(mainnet, 100);

        // Locally bump the object to version 6.
        overlay.apply_written(object(id, 6));

        let hit = get_one(&overlay, at_checkpoint(id)).expect("overlay should win");
        assert_eq!(hit.1, 6, "current read must see the local version");
    }

    #[test]
    fn exact_historical_version_still_comes_from_mainnet() {
        let id = ObjectID::random();
        let mut mainnet = MockMainnet::default();
        mainnet.insert(object(id, 5));
        let overlay = OverlayStore::new(mainnet, 100);
        overlay.apply_written(object(id, 6));

        // The overlay holds v6, but an explicit request for v5 is historical.
        let hit = get_one(&overlay, at_version(id, 5)).expect("mainnet keeps history");
        assert_eq!(hit.1, 5);
        // And the overlay's own version is served when asked for exactly.
        let hit6 = get_one(&overlay, at_version(id, 6)).expect("overlay serves its version");
        assert_eq!(hit6.1, 6);
    }

    #[test]
    fn tombstone_hides_object_from_current_reads_only() {
        let id = ObjectID::random();
        let mut mainnet = MockMainnet::default();
        mainnet.insert(object(id, 5));
        let overlay = OverlayStore::new(mainnet, 100);

        overlay.tombstone(id);
        assert!(
            get_one(&overlay, at_checkpoint(id)).is_none(),
            "deleted object must be absent for current reads"
        );
        // A historical version remains addressable on mainnet.
        assert!(get_one(&overlay, at_version(id, 5)).is_some());
    }

    #[test]
    fn batch_results_preserve_key_order() {
        let (a, b, c) = (ObjectID::random(), ObjectID::random(), ObjectID::random());
        let mut mainnet = MockMainnet::default();
        mainnet.insert(object(a, 1));
        mainnet.insert(object(c, 3));
        let overlay = OverlayStore::new(mainnet, 100);
        overlay.apply_written(object(b, 9));

        let keys = vec![at_checkpoint(a), at_checkpoint(b), at_checkpoint(c)];
        let results = overlay.get_objects(&keys).unwrap();
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].as_ref().unwrap().1, 1, "a from mainnet");
        assert_eq!(results[1].as_ref().unwrap().1, 9, "b from overlay");
        assert_eq!(results[2].as_ref().unwrap().1, 3, "c from mainnet");
    }

    #[test]
    fn apply_written_clears_prior_tombstone() {
        let id = ObjectID::random();
        let overlay = OverlayStore::new(MockMainnet::default(), 100);
        overlay.tombstone(id);
        overlay.apply_written(object(id, 7));
        let hit = get_one(&overlay, at_checkpoint(id)).expect("resurrected by write");
        assert_eq!(hit.1, 7);
    }

    #[cfg(feature = "execute")]
    #[test]
    fn snapshot_and_restore_roll_back_the_overlay() {
        let (a, b) = (ObjectID::random(), ObjectID::random());
        let overlay = OverlayStore::new(MockMainnet::default(), 100);
        overlay.apply_written(object(a, 1));

        let snap = overlay.snapshot();

        // Mutate past the snapshot: add b, tombstone a.
        overlay.apply_written(object(b, 2));
        overlay.tombstone(a);
        assert!(get_one(&overlay, at_checkpoint(a)).is_none());
        assert!(get_one(&overlay, at_checkpoint(b)).is_some());

        overlay.restore(snap);

        // Back to just a@v1; b is gone and a's tombstone is cleared.
        assert_eq!(get_one(&overlay, at_checkpoint(a)).unwrap().1, 1);
        assert!(get_one(&overlay, at_checkpoint(b)).is_none());
    }

    #[cfg(feature = "execute")]
    #[test]
    fn restore_is_repeatable_from_one_snapshot() {
        let id = ObjectID::random();
        let overlay = OverlayStore::new(MockMainnet::default(), 100);
        overlay.apply_written(object(id, 4));
        let snap = overlay.snapshot();
        for _ in 0..3 {
            overlay.apply_written(object(id, 9));
            assert_eq!(get_one(&overlay, at_checkpoint(id)).unwrap().1, 9);
            overlay.restore(snap.clone());
            assert_eq!(get_one(&overlay, at_checkpoint(id)).unwrap().1, 4);
        }
    }

    #[cfg(feature = "serve")]
    #[test]
    fn export_import_round_trips_objects_and_tombstones() {
        let (a, b) = (ObjectID::random(), ObjectID::random());
        let src = OverlayStore::new(MockMainnet::default(), 100);
        src.apply_written(object(a, 7));
        src.tombstone(b);

        let persisted = src.export();

        let dst = OverlayStore::new(MockMainnet::default(), 100);
        dst.import(persisted);
        // a@v7 came across; b's tombstone hides it from current reads.
        assert_eq!(get_one(&dst, at_checkpoint(a)).unwrap().1, 7);
        assert!(get_one(&dst, at_checkpoint(b)).is_none());
        assert!(dst.overlay_touched_ids().contains(&b), "tombstone imported");
    }
}
