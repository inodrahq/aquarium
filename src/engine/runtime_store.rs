// Portions Copyright (c) Mysten Labs, Inc.
// Copyright (c) Inodra (modifications)
// SPDX-License-Identifier: Apache-2.0
//
// This file is ADAPTED FROM Mysten Labs' `sui-replay-2` crate
// (crates/sui-replay-2/src/execution.rs, the private `ReplayStore`), which is
// licensed under the Apache License, Version 2.0. It exposes a
// `sui_data_store::ObjectStore` to the Sui `Executor` through the runtime
// storage traits the VM requires. See the NOTICE file for attribution.

//! Adapter from a `sui_data_store::ObjectStore` to the Sui runtime storage
//! traits (`BackingStore`).
//!
//! During execution the Move VM resolves objects, packages and dynamic-field
//! children through these traits. [`RuntimeStore`] services each request by
//! reading through to the underlying data store (the fork overlay), pinning
//! "current" reads to the fork checkpoint, and caching by `(id, version)`.

use std::cell::RefCell;
use std::collections::BTreeMap;

use sui_data_store::{ObjectKey, ObjectStore as DataObjectStore, VersionQuery};
use sui_types::base_types::{ObjectID, ObjectRef, SequenceNumber, VersionNumber};
use sui_types::committee::EpochId;
use sui_types::error::{SuiErrorKind, SuiResult};
use sui_types::object::Object;
use sui_types::storage::{
    BackingPackageStore, ChildObjectResolver, ObjectStore, PackageObject, ParentSync,
};

/// Runtime-store adapter over a fork's data store, pinned at a checkpoint.
pub struct RuntimeStore<'a> {
    store: &'a dyn DataObjectStore,
    object_cache: RefCell<BTreeMap<ObjectID, BTreeMap<u64, Object>>>,
    checkpoint: u64,
}

impl<'a> RuntimeStore<'a> {
    /// Wrap a data store, anchoring "current" reads at `checkpoint`.
    pub fn new(store: &'a dyn DataObjectStore, checkpoint: u64) -> Self {
        Self {
            store,
            object_cache: RefCell::new(BTreeMap::new()),
            checkpoint,
        }
    }

    fn cache_insert(&self, obj: &Object) {
        self.object_cache
            .borrow_mut()
            .entry(obj.id())
            .or_default()
            .insert(obj.version().value(), obj.clone());
    }

    fn get_object_at_version(
        &self,
        object_id: &ObjectID,
        version: VersionNumber,
    ) -> Option<Object> {
        if let Some(obj) = self
            .object_cache
            .borrow()
            .get(object_id)
            .and_then(|versions| versions.get(&version.value()).cloned())
        {
            return Some(obj);
        }
        let object = self
            .store
            .get_objects(&[ObjectKey {
                object_id: *object_id,
                version_query: VersionQuery::Version(version.value()),
            }])
            .ok()?
            .into_iter()
            .next()?
            .map(|(obj, _version)| obj);
        if let Some(obj) = &object {
            self.cache_insert(obj);
        }
        object
    }
}

impl BackingPackageStore for RuntimeStore<'_> {
    fn get_package_object(&self, package_id: &ObjectID) -> SuiResult<Option<PackageObject>> {
        if let Some(versions) = self.object_cache.borrow().get(package_id) {
            // Packages are immutable: at most one version is ever cached.
            if let Some(obj) = versions.values().next() {
                return Ok(Some(PackageObject::new(obj.clone())));
            }
        }
        let fetched = self
            .store
            .get_objects(&[ObjectKey {
                object_id: *package_id,
                version_query: VersionQuery::AtCheckpoint(self.checkpoint),
            }])
            .map_err(|e| SuiErrorKind::Storage(e.to_string()))?;
        match fetched.into_iter().next().flatten() {
            Some((package, _version)) => {
                self.cache_insert(&package);
                Ok(Some(PackageObject::new(package)))
            }
            None => Ok(None),
        }
    }
}

impl ObjectStore for RuntimeStore<'_> {
    fn get_object(&self, object_id: &ObjectID) -> Option<Object> {
        if let Some(versions) = self.object_cache.borrow().get(object_id) {
            return versions.last_key_value().map(|(_v, obj)| obj.clone());
        }
        let fetched = self
            .store
            .get_objects(&[ObjectKey {
                object_id: *object_id,
                version_query: VersionQuery::AtCheckpoint(self.checkpoint),
            }])
            .ok()?
            .into_iter()
            .next()?
            .map(|(obj, _version)| obj)?;
        self.cache_insert(&fetched);
        Some(fetched)
    }

    fn get_object_by_key(&self, object_id: &ObjectID, version: VersionNumber) -> Option<Object> {
        self.get_object_at_version(object_id, version)
    }
}

impl ChildObjectResolver for RuntimeStore<'_> {
    fn read_child_object(
        &self,
        _parent: &ObjectID,
        child: &ObjectID,
        child_version_upper_bound: SequenceNumber,
    ) -> SuiResult<Option<Object>> {
        let fetched = self
            .store
            .get_objects(&[ObjectKey {
                object_id: *child,
                version_query: VersionQuery::RootVersion(child_version_upper_bound.value()),
            }])
            .map_err(|e| SuiErrorKind::Storage(e.to_string()))?;
        let object = fetched.into_iter().next().flatten().map(|(obj, _v)| obj);
        if let Some(obj) = &object {
            self.cache_insert(obj);
        }
        Ok(object)
    }

    fn get_object_received_at_version(
        &self,
        _owner: &ObjectID,
        receiving_object_id: &ObjectID,
        receive_object_at_version: SequenceNumber,
        _epoch_id: EpochId,
    ) -> SuiResult<Option<Object>> {
        Ok(self.get_object_at_version(receiving_object_id, receive_object_at_version))
    }
}

// The VM never drives these in the execution paths Aquarium uses; match the
// upstream replay behaviour of failing loudly rather than guessing.

impl ParentSync for RuntimeStore<'_> {
    fn get_latest_parent_entry_ref_deprecated(&self, object_id: ObjectID) -> Option<ObjectRef> {
        unreachable!("unexpected ParentSync::get_latest_parent_entry_ref_deprecated({object_id})")
    }
}
