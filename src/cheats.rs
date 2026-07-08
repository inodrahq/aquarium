// Copyright (c) Inodra
// SPDX-License-Identifier: Apache-2.0

//! Fork "cheat" controls — the anvil analog of `evm_setTime` / `evm_mine` /
//! `anvil_setStorageAt`.
//!
//! A fork is frozen at a checkpoint: it has no consensus to advance the on-chain
//! `Clock` (`0x6`), cross an epoch boundary, or refresh the randomness beacon.
//! These helpers let a developer drive that state directly against the overlay,
//! so time-, epoch- and oracle-gated code paths can be tested locally:
//!
//! - [`set_clock_timestamp_ms`] / [`advance_clock_ms`] and the [`ClockMode`]
//!   auto-drift (the server stamps the clock to real wall time before each tx),
//! - [`set_object_contents`], the general object override that lets you inject a
//!   fresh oracle price (e.g. into a Pyth `PriceInfoObject`) or poke any other
//!   Move object the fork can't advance on its own.
//!
//! Epoch advance is a property of the VM (see [`crate::engine::Vm::advance_epoch`]);
//! the server ties it together with a clock jump.
//!
//! These are deliberately **not** part of the `sui.rpc.v2` gRPC surface (which
//! stays a faithful node twin) — the server exposes them over a small JSON HTTP
//! control API instead.

use std::time::Instant;

use anyhow::{Context, Result, bail};
use sui_data_store::ObjectStore;
use sui_types::SUI_CLOCK_OBJECT_ID;
use sui_types::base_types::{ObjectID, SequenceNumber};
use sui_types::clock::Clock;
use sui_types::digests::TransactionDigest;
use sui_types::object::Object;

use crate::fork::Fork;

/// How the fork drives the on-chain `Clock` (`0x6`).
#[derive(Clone)]
pub enum ClockMode {
    /// Track real wall-clock drift from an anchor: the clock the fork presents
    /// is `anchor_ms + time since anchor_at`. This is the default, so a freshly
    /// forked chain advances time like the real one instead of freezing.
    Auto { anchor_ms: u64, anchor_at: Instant },
    /// Hold a fixed timestamp (set via the control API); never auto-advances.
    Fixed(u64),
    /// Leave the clock exactly where it is; the server never auto-writes it.
    Frozen,
}

impl ClockMode {
    /// The timestamp this mode wants the clock at *now* (`None` for `Frozen`).
    pub fn target_ms(&self) -> Option<u64> {
        match self {
            ClockMode::Auto {
                anchor_ms,
                anchor_at,
            } => Some(anchor_ms.saturating_add(anchor_at.elapsed().as_millis() as u64)),
            ClockMode::Fixed(ms) => Some(*ms),
            ClockMode::Frozen => None,
        }
    }
}

/// Read the `Clock` (`0x6`) timestamp as the fork currently sees it.
pub fn clock_timestamp_ms<S: ObjectStore>(fork: &Fork<S>) -> Result<u64> {
    let obj = fork
        .object(SUI_CLOCK_OBJECT_ID)?
        .context("clock object 0x6 not found on the fork")?;
    let move_obj = obj
        .data
        .try_as_move()
        .context("clock object 0x6 is not a Move object")?;
    let clock: Clock = bcs::from_bytes(move_obj.contents()).context("decoding Clock contents")?;
    Ok(clock.timestamp_ms)
}

/// Set the `Clock` (`0x6`) to `timestamp_ms`, writing a new version into the
/// overlay. A no-op (no version churn) if the clock is already at that value.
pub fn set_clock_timestamp_ms<S: ObjectStore>(fork: &Fork<S>, timestamp_ms: u64) -> Result<()> {
    let obj = fork
        .object(SUI_CLOCK_OBJECT_ID)?
        .context("clock object 0x6 not found on the fork")?;
    let mut move_obj = obj
        .data
        .try_as_move()
        .context("clock object 0x6 is not a Move object")?
        .clone();
    let mut clock: Clock =
        bcs::from_bytes(move_obj.contents()).context("decoding Clock contents")?;
    if clock.timestamp_ms == timestamp_ms {
        return Ok(());
    }
    clock.timestamp_ms = timestamp_ms;
    let bytes = bcs::to_bytes(&clock).context("encoding Clock contents")?;
    move_obj.set_contents_unsafe(bytes);
    let next = SequenceNumber::from(obj.version().value().saturating_add(1));
    move_obj.increment_version_to(next);
    let new_object = Object::new_move(move_obj, obj.owner().clone(), TransactionDigest::default());
    fork.store().set_object(new_object);
    Ok(())
}

/// Advance the `Clock` by `delta_ms` and return the new timestamp.
pub fn advance_clock_ms<S: ObjectStore>(fork: &Fork<S>, delta_ms: u64) -> Result<u64> {
    let next = clock_timestamp_ms(fork)?.saturating_add(delta_ms);
    set_clock_timestamp_ms(fork, next)?;
    Ok(next)
}

/// Overwrite an existing object's Move contents in the overlay — the fork's
/// `setStorageAt`. Preserves the object's type and owner and (by default) bumps
/// its version, so the next read/transaction sees the new bytes.
///
/// The caller supplies the full new BCS contents (they know the target's Move
/// layout — e.g. a Pyth `PriceInfoObject` whose price field they want fresh).
/// As a guard against corrupting the overlay, the contents must still begin with
/// the object's own `UID` (its 32-byte id); a package or absent object is an
/// error. Returns the object's version after the write.
pub fn set_object_contents<S: ObjectStore>(
    fork: &Fork<S>,
    id: ObjectID,
    contents: Vec<u8>,
    bump_version: bool,
) -> Result<u64> {
    let obj = fork
        .object(id)?
        .with_context(|| format!("object {id} not found on the fork"))?;
    let mut move_obj = obj
        .data
        .try_as_move()
        .with_context(|| format!("object {id} is a package, not a Move object"))?
        .clone();

    // A Move object's contents start with its `UID` (== its ObjectID). Reject a
    // blob for the wrong id rather than silently corrupt the overlay.
    if contents.len() < ObjectID::LENGTH {
        bail!(
            "override contents for {id} are too short ({} bytes) to contain a UID",
            contents.len()
        );
    }
    let embedded = ObjectID::from_bytes(&contents[..ObjectID::LENGTH])
        .map_err(|e| anyhow::anyhow!("override contents for {id} have an unreadable UID: {e}"))?;
    if embedded != id {
        bail!("override contents carry UID {embedded}, which does not match target object {id}");
    }

    move_obj.set_contents_unsafe(contents);
    let version = if bump_version {
        let next = obj.version().value().saturating_add(1);
        move_obj.increment_version_to(SequenceNumber::from(next));
        next
    } else {
        obj.version().value()
    };
    let new_object = Object::new_move(move_obj, obj.owner().clone(), TransactionDigest::default());
    fork.store().set_object(new_object);
    Ok(version)
}

/// Mint a fresh `Coin<coin_type>` of `amount` owned by `address` directly into
/// the overlay — the anvil `setBalance` analog, so a developer gets a funded
/// test account without impersonating a whale. The coin is fabricated (a fork
/// bypasses supply invariants), so this can fund any coin type. Returns the new
/// coin's object id.
pub fn fund<S: ObjectStore>(
    fork: &Fork<S>,
    address: sui_types::base_types::SuiAddress,
    amount: u64,
    coin_type: sui_types::TypeTag,
) -> Result<ObjectID> {
    use sui_types::base_types::MoveObjectType;
    use sui_types::coin::Coin;
    use sui_types::object::{MoveObject, Owner};

    // Generous size bound; our contents are ~40 bytes (UID + u64 balance).
    const MAX_MOVE_OBJECT_SIZE: u64 = 256 * 1024;

    let id = ObjectID::random();
    let contents = bcs::to_bytes(&Coin::new(id, amount)).context("encoding Coin contents")?;
    let move_type: MoveObjectType = Coin::type_(coin_type).into();
    // Safe: `Coin<T>` always has `store` (public transfer), matching the flag.
    let move_obj = unsafe {
        MoveObject::new_from_execution_with_limit(
            move_type,
            /* has_public_transfer */ true,
            SequenceNumber::from(1),
            contents,
            MAX_MOVE_OBJECT_SIZE,
        )
    }
    .map_err(|e| anyhow::anyhow!("building coin object: {e}"))?;
    let object = Object::new_move(
        move_obj,
        Owner::AddressOwner(address),
        TransactionDigest::default(),
    );
    fork.store().set_object(object);
    Ok(id)
}

type SystemStateField = sui_types::dynamic_field::Field<
    u64,
    sui_types::sui_system_state::sui_system_state_inner_v2::SuiSystemStateInnerV2,
>;

/// Resolve and decode the on-chain `SuiSystemState` (`0x5`) inner state as the
/// fork currently sees it, returning `(inner field object id, decoded field)`.
///
/// Only the current (V2) system-state layout is supported; anything else is an
/// error. The inner state lives as a dynamic field of `0x5` keyed by the
/// wrapper's `version`.
fn load_system_state_inner<S: ObjectStore>(fork: &Fork<S>) -> Result<(ObjectID, SystemStateField)> {
    use sui_types::SUI_SYSTEM_STATE_OBJECT_ID;
    use sui_types::TypeTag;
    use sui_types::dynamic_field::derive_dynamic_field_id;
    use sui_types::sui_system_state::SuiSystemStateWrapper;

    let wrapper_obj = fork
        .object(SUI_SYSTEM_STATE_OBJECT_ID)?
        .context("system state object 0x5 not found on the fork")?;
    let wrapper: SuiSystemStateWrapper = bcs::from_bytes(
        wrapper_obj
            .data
            .try_as_move()
            .context("0x5 is not a Move object")?
            .contents(),
    )
    .context("decoding SuiSystemStateWrapper")?;

    let key_bytes = bcs::to_bytes(&wrapper.version)?;
    let child_id = derive_dynamic_field_id(SUI_SYSTEM_STATE_OBJECT_ID, &TypeTag::U64, &key_bytes)
        .context("deriving system-state inner field id")?;
    let child_obj = fork
        .object(child_id)?
        .with_context(|| format!("system-state inner field {child_id} not found"))?;
    let field: SystemStateField = bcs::from_bytes(
        child_obj
            .data
            .try_as_move()
            .context("system-state inner field is not a Move object")?
            .contents(),
    )
    .context("decoding SuiSystemStateInnerV2 (unsupported system-state version?)")?;
    Ok((child_id, field))
}

/// The `epoch` recorded in the on-chain `SuiSystemState` (`0x5`) as the fork
/// sees it (`None` if the layout is unsupported / unreadable).
pub fn system_state_epoch<S: ObjectStore>(fork: &Fork<S>) -> Option<u64> {
    load_system_state_inner(fork)
        .ok()
        .map(|(_, f)| f.value.epoch)
}

/// Update the on-chain `SuiSystemState` (`0x5`) so its inner `epoch` and
/// `epoch_start_timestamp_ms` agree with a shallow epoch advance, for protocols
/// that read the system state's epoch rather than `TxContext`.
///
/// Best-effort and deliberately minimal: only the current (V2) system-state
/// layout is handled (anything else bails, and the VM's `TxContext` epoch has
/// still advanced), and it does **not** settle staking rewards, grow validator
/// exchange rates, or rotate the validator set — it only moves the epoch counter
/// and start timestamp forward so `sui_system::epoch()` matches.
pub fn sync_system_state_epoch<S: ObjectStore>(
    fork: &Fork<S>,
    epoch: u64,
    epoch_start_timestamp_ms: u64,
) -> Result<()> {
    let (child_id, mut field) = load_system_state_inner(fork)?;
    field.value.epoch = epoch;
    field.value.epoch_start_timestamp_ms = epoch_start_timestamp_ms;
    let bytes = bcs::to_bytes(&field).context("re-encoding system-state inner")?;

    // Write the inner field back (preserves its `ObjectOwner(0x5)` + bumps
    // version); the UID prefix guard passes since a `Field`'s first bytes are
    // its own id.
    set_object_contents(fork, child_id, bytes, true)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_mode_auto_advances_from_anchor() {
        // A tiny sleep should push the target strictly past the anchor.
        let mode = ClockMode::Auto {
            anchor_ms: 1_000,
            anchor_at: Instant::now(),
        };
        std::thread::sleep(std::time::Duration::from_millis(5));
        let t = mode.target_ms().expect("auto has a target");
        assert!(t >= 1_000, "never goes below the anchor");
    }

    #[test]
    fn clock_mode_fixed_and_frozen() {
        assert_eq!(ClockMode::Fixed(42).target_ms(), Some(42));
        assert_eq!(ClockMode::Frozen.target_ms(), None);
    }

    // --- cheats that mutate a fork overlay ---
    // A `for_testing` Move object's contents are a `Coin` (UID + u64), which is
    // byte-identical to a `Clock` (UID + `timestamp_ms`), so these tests can
    // exercise the clock cheats and the object-override guard without a network.

    use sui_data_store::ObjectKey;
    use sui_types::base_types::{SequenceNumber, SuiAddress};
    use sui_types::object::Owner;

    /// Backing store that knows nothing — every read is a miss, so the overlay
    /// is the only source of truth (which is what we want to drive here).
    struct Empty;
    impl sui_data_store::ObjectStore for Empty {
        fn get_objects(
            &self,
            keys: &[ObjectKey],
        ) -> std::result::Result<Vec<Option<(Object, u64)>>, anyhow::Error> {
            Ok(keys.iter().map(|_| None).collect())
        }
    }

    fn test_object(id: ObjectID, version: u64) -> Object {
        Object::with_id_owner_version_for_testing(
            id,
            SequenceNumber::from(version),
            Owner::AddressOwner(SuiAddress::ZERO),
        )
    }

    #[test]
    fn clock_set_and_advance_round_trip() {
        let fork = Fork::with_store(Empty, 100);
        fork.store().set_object(test_object(SUI_CLOCK_OBJECT_ID, 5));

        set_clock_timestamp_ms(&fork, 1_000).unwrap();
        assert_eq!(clock_timestamp_ms(&fork).unwrap(), 1_000);

        // Version was bumped past the original (5).
        let v = fork
            .object(SUI_CLOCK_OBJECT_ID)
            .unwrap()
            .unwrap()
            .version()
            .value();
        assert!(v > 5, "clock write bumps the object version");

        assert_eq!(advance_clock_ms(&fork, 250).unwrap(), 1_250);
        assert_eq!(clock_timestamp_ms(&fork).unwrap(), 1_250);
    }

    #[test]
    fn set_clock_is_a_noop_when_unchanged() {
        let fork = Fork::with_store(Empty, 100);
        fork.store().set_object(test_object(SUI_CLOCK_OBJECT_ID, 5));
        set_clock_timestamp_ms(&fork, 9_000).unwrap();
        let v1 = fork
            .object(SUI_CLOCK_OBJECT_ID)
            .unwrap()
            .unwrap()
            .version()
            .value();
        // Setting the same value again must not churn the version.
        set_clock_timestamp_ms(&fork, 9_000).unwrap();
        let v2 = fork
            .object(SUI_CLOCK_OBJECT_ID)
            .unwrap()
            .unwrap()
            .version()
            .value();
        assert_eq!(v1, v2);
    }

    #[test]
    fn set_object_contents_guards_and_bumps() {
        let id = ObjectID::random();
        let fork = Fork::with_store(Empty, 100);
        fork.store().set_object(test_object(id, 3));

        let uid = id.into_bytes().to_vec();
        let valid = || {
            let mut c = uid.clone();
            c.extend(1_234u64.to_le_bytes());
            c
        };

        // Too short to hold a UID.
        assert!(set_object_contents(&fork, id, vec![0u8; 10], true).is_err());
        // UID prefix does not match the target id.
        let mut wrong = ObjectID::random().into_bytes().to_vec();
        wrong.extend(0u64.to_le_bytes());
        assert!(set_object_contents(&fork, id, wrong, true).is_err());

        // Valid override bumps the version 3 -> 4.
        assert_eq!(set_object_contents(&fork, id, valid(), true).unwrap(), 4);
        // Without bump, the version is preserved.
        assert_eq!(set_object_contents(&fork, id, valid(), false).unwrap(), 4);
    }

    #[test]
    fn set_object_contents_rejects_missing_object() {
        let fork = Fork::with_store(Empty, 100);
        let id = ObjectID::random();
        let mut c = id.into_bytes().to_vec();
        c.extend(0u64.to_le_bytes());
        assert!(set_object_contents(&fork, id, c, true).is_err());
    }

    #[test]
    fn fund_mints_a_coin_owned_by_the_address_with_the_right_balance() {
        let fork = Fork::with_store(Empty, 100);
        let addr = SuiAddress::ZERO;
        let sui = sui_types::parse_sui_type_tag("0x2::sui::SUI").unwrap();

        let id = fund(&fork, addr, 777, sui).unwrap();
        let obj = fork.object(id).unwrap().expect("funded coin exists");

        assert!(
            matches!(obj.owner(), Owner::AddressOwner(a) if *a == addr),
            "owned by the funded address"
        );
        let (ty, value) = sui_types::coin::Coin::extract_balance_if_coin(&obj)
            .unwrap()
            .expect("is a coin");
        assert_eq!(value, 777);
        assert_eq!(ty, sui_types::parse_sui_type_tag("0x2::sui::SUI").unwrap());
    }
}
