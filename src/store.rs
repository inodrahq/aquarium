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
#[derive(Default)]
struct OverlayState {
    /// Objects created or mutated by locally executed transactions, keyed by id
    /// and holding the *latest* local version.
    objects: BTreeMap<ObjectID, Object>,
    /// Objects deleted by locally executed transactions.
    tombstones: BTreeSet<ObjectID>,
    /// Transactions executed locally, by digest (Base58).
    transactions: BTreeMap<String, TransactionInfo>,
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
    /// are inserted (clearing any tombstone), deleted ids are tombstoned, and
    /// the transaction is recorded — all under one lock.
    ///
    /// Crate-internal: the supported way to mutate a fork is [`Fork::execute`],
    /// which serializes commits. Direct callers would bypass that serialization.
    #[cfg(feature = "execute")]
    pub(crate) fn commit(
        &self,
        written: &[Object],
        deleted: &[ObjectID],
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
        state.transactions.insert(digest, info);
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
            let fetched = self.inner.get_objects(&miss_keys)?;
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
        self.inner.epoch_info(epoch)
    }

    fn protocol_config(&self, epoch: u64) -> Result<Option<ProtocolConfig>, Error> {
        self.inner.protocol_config(epoch)
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
        self.inner.transaction_data_and_effects(tx_digest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sui_types::base_types::{SequenceNumber, SuiAddress};
    use sui_types::object::Owner;

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
}
