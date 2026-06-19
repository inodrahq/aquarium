// Copyright (c) Inodra
// SPDX-License-Identifier: Apache-2.0

//! Transaction execution against a fork (feature `execute`).
//!
//! The Move VM and adapter come from `sui-execution`; [`runtime_store`] adapts
//! the fork's data store to the runtime storage traits, and [`vm::Vm`] drives a
//! single transaction and reports the overlay mutations to commit.

pub mod runtime_store;
pub mod vm;

pub use vm::{ExecutionOutcome, Vm};
