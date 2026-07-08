// Copyright (c) Inodra
// SPDX-License-Identifier: Apache-2.0

//! # Aquarium
//!
//! Fork Sui mainnet locally — a contained, observable slice of the chain.
//!
//! Aquarium takes live mainnet object state (fetched lazily through Mysten's
//! GraphQL endpoint and cached) and lets you execute new transactions against
//! it in a local overlay, with no validators, consensus, or snapshot. The real
//! chain is never touched.
//!
//! See `DESIGN.md` for the architecture. The core types are [`fork::Fork`] (the
//! handle) and [`store::OverlayStore`] (the writable overlay). Transaction
//! execution lives behind the `execute` feature in [`engine`].

pub mod fork;
pub mod gql;
pub mod store;

#[cfg(feature = "execute")]
pub mod engine;

#[cfg(feature = "serve")]
pub mod cheats;

#[cfg(feature = "serve")]
pub mod control;

#[cfg(feature = "serve")]
pub mod movedesc;

#[cfg(feature = "serve")]
pub mod serve;

pub use fork::{Fork, MainnetFork};
pub use gql::Gql;
pub use store::OverlayStore;
