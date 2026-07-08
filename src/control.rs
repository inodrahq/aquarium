// Copyright (c) Inodra
// SPDX-License-Identifier: Apache-2.0

//! The fork "cheat" control API — a small JSON/HTTP surface (anvil-style) for
//! driving frozen fork state: advance the clock, cross an epoch boundary, or
//! override an object.
//!
//! It is served on its own port, separate from the `sui.rpc.v2` gRPC endpoint,
//! so that surface stays a faithful node twin. Everything here is a developer
//! convenience against a *local* fork; there is no auth (the fork already lets
//! any account be impersonated).
//!
//! Endpoints (all POST take/return JSON; times are unix-epoch milliseconds):
//! - `GET  /status`               — clock, epoch and fork summary
//! - `POST /clock/set`            — `{ "timestamp_ms": N }`  pin the clock
//! - `POST /clock/advance`        — `{ "delta_ms": N }`      bump the clock
//! - `POST /clock/auto`           — resume real wall-clock drift
//! - `POST /clock/freeze`         — stop auto-advancing the clock
//! - `POST /epoch/advance`        — `{ "count": N?, "timestamp_ms": N? }`
//! - `POST /object/set_contents`  — `{ "object_id", "contents_base64", "bump_version"? }`
//! - `POST /snapshot`             — capture fork state, returns `{ "id": N }`
//! - `POST /revert`               — `{ "id": N }`  roll back to a snapshot
//! - `POST /state/dump`           — `{ "path": "…" }`  write fork state to disk
//! - `POST /state/load`           — `{ "path": "…" }`  reload fork state
//! - `POST /trace`                — `{ "transaction": "<b64>", "full"? }`  dry-run + trace

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use sui_types::base_types::ObjectID;

use crate::cheats::ClockMode;
use crate::serve::{ForkState, b64_decode};

/// A fork epoch's nominal duration (mainnet epochs are ~24h). Used to pick a
/// default clock jump when advancing the epoch without an explicit timestamp.
const EPOCH_MS: u64 = 24 * 60 * 60 * 1000;

/// Build the cheat-control router bound to the shared fork state.
pub(crate) fn router(state: Arc<ForkState>) -> Router {
    Router::new()
        .route("/status", get(status))
        .route("/clock/set", post(clock_set))
        .route("/clock/advance", post(clock_advance))
        .route("/clock/auto", post(clock_auto))
        .route("/clock/freeze", post(clock_freeze))
        .route("/epoch/advance", post(epoch_advance))
        .route("/object/set_contents", post(object_set_contents))
        .route("/snapshot", post(snapshot))
        .route("/revert", post(revert))
        .route("/state/dump", post(state_dump))
        .route("/state/load", post(state_load))
        .route("/trace", post(trace))
        .with_state(state)
}

type ApiError = (StatusCode, String);

fn ise(e: impl std::fmt::Display) -> ApiError {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

fn bad(e: impl std::fmt::Display) -> ApiError {
    (StatusCode::BAD_REQUEST, e.to_string())
}

/// Run a blocking closure on the blocking pool, flattening join + inner errors.
async fn blocking<T, F>(f: F) -> Result<T, ApiError>
where
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(ise)?
        .map_err(ise)
}

fn clock_label(mode: &ClockMode) -> &'static str {
    match mode {
        ClockMode::Auto { .. } => "auto",
        ClockMode::Fixed(_) => "fixed",
        ClockMode::Frozen => "frozen",
    }
}

#[derive(Serialize)]
struct Status {
    fork_checkpoint: u64,
    epoch: u64,
    /// Epoch recorded in the on-chain SuiSystemState (0x5); should match `epoch`.
    system_state_epoch: Option<u64>,
    epoch_start_timestamp_ms: u64,
    clock_timestamp_ms: u64,
    clock_mode: String,
    executed_transactions: u64,
}

async fn status(State(state): State<Arc<ForkState>>) -> Result<Json<Status>, ApiError> {
    let clock_mode =
        clock_label(&state.clock.lock().unwrap_or_else(|p| p.into_inner())).to_string();
    let s = state.clone();
    let (clock_timestamp_ms, system_state_epoch) = blocking(move || {
        Ok((
            crate::cheats::clock_timestamp_ms(&s.fork)?,
            crate::cheats::system_state_epoch(&s.fork),
        ))
    })
    .await?;
    Ok(Json(Status {
        fork_checkpoint: state.fork.fork_checkpoint(),
        epoch: state.epoch(),
        system_state_epoch,
        epoch_start_timestamp_ms: state.vm.epoch_start_timestamp_ms(),
        clock_timestamp_ms,
        clock_mode,
        executed_transactions: state.fork.store().executed_count() as u64,
    }))
}

#[derive(Deserialize)]
struct SetClock {
    timestamp_ms: u64,
}

#[derive(Serialize)]
struct ClockResult {
    clock_timestamp_ms: u64,
    clock_mode: String,
}

async fn clock_set(
    State(state): State<Arc<ForkState>>,
    Json(req): Json<SetClock>,
) -> Result<Json<ClockResult>, ApiError> {
    let s = state.clone();
    let ms = req.timestamp_ms;
    blocking(move || crate::cheats::set_clock_timestamp_ms(&s.fork, ms)).await?;
    // Pinning the clock switches off auto-drift (like anvil's evm_setTime).
    *state.clock.lock().unwrap_or_else(|p| p.into_inner()) = ClockMode::Fixed(ms);
    Ok(Json(ClockResult {
        clock_timestamp_ms: ms,
        clock_mode: "fixed".to_string(),
    }))
}

#[derive(Deserialize)]
struct AdvanceClock {
    delta_ms: u64,
}

async fn clock_advance(
    State(state): State<Arc<ForkState>>,
    Json(req): Json<AdvanceClock>,
) -> Result<Json<ClockResult>, ApiError> {
    let s = state.clone();
    let delta = req.delta_ms;
    let new_ms = blocking(move || crate::cheats::advance_clock_ms(&s.fork, delta)).await?;
    *state.clock.lock().unwrap_or_else(|p| p.into_inner()) = ClockMode::Fixed(new_ms);
    Ok(Json(ClockResult {
        clock_timestamp_ms: new_ms,
        clock_mode: "fixed".to_string(),
    }))
}

async fn clock_auto(State(state): State<Arc<ForkState>>) -> Result<Json<ClockResult>, ApiError> {
    let s = state.clone();
    let now_ms = blocking(move || crate::cheats::clock_timestamp_ms(&s.fork)).await?;
    *state.clock.lock().unwrap_or_else(|p| p.into_inner()) = ClockMode::Auto {
        anchor_ms: now_ms,
        anchor_at: std::time::Instant::now(),
    };
    Ok(Json(ClockResult {
        clock_timestamp_ms: now_ms,
        clock_mode: "auto".to_string(),
    }))
}

async fn clock_freeze(State(state): State<Arc<ForkState>>) -> Result<Json<ClockResult>, ApiError> {
    let s = state.clone();
    let now_ms = blocking(move || crate::cheats::clock_timestamp_ms(&s.fork)).await?;
    *state.clock.lock().unwrap_or_else(|p| p.into_inner()) = ClockMode::Frozen;
    Ok(Json(ClockResult {
        clock_timestamp_ms: now_ms,
        clock_mode: "frozen".to_string(),
    }))
}

#[derive(Deserialize)]
struct AdvanceEpoch {
    /// Number of epochs to cross (default 1).
    count: Option<u64>,
    /// New epoch-start timestamp (default: current clock + count × ~24h).
    timestamp_ms: Option<u64>,
}

#[derive(Serialize)]
struct EpochResult {
    epoch: u64,
    epoch_start_timestamp_ms: u64,
    clock_timestamp_ms: u64,
    note: String,
}

async fn epoch_advance(
    State(state): State<Arc<ForkState>>,
    Json(req): Json<AdvanceEpoch>,
) -> Result<Json<EpochResult>, ApiError> {
    let count = req.count.unwrap_or(1).max(1);
    let s = state.clone();
    let (epoch, new_ts) = blocking(move || -> anyhow::Result<(u64, u64)> {
        let current = crate::cheats::clock_timestamp_ms(&s.fork)?;
        let new_ts = req
            .timestamp_ms
            .unwrap_or_else(|| current.saturating_add(count.saturating_mul(EPOCH_MS)));
        let epoch = s.vm.advance_epoch(count, new_ts);
        // Move time forward to the new epoch boundary too, so time- and
        // epoch-gated logic stay consistent.
        crate::cheats::set_clock_timestamp_ms(&s.fork, new_ts)?;
        // Best-effort: also move the on-chain SuiSystemState (0x5) epoch forward
        // so protocols that read it (not just TxContext) stay consistent. A
        // failure here (e.g. an unsupported system-state layout) is non-fatal —
        // the VM epoch has already advanced.
        if let Err(e) = crate::cheats::sync_system_state_epoch(&s.fork, epoch, new_ts) {
            tracing::warn!("SuiSystemState (0x5) epoch sync skipped: {e}");
        }
        Ok((epoch, new_ts))
    })
    .await?;
    // Keep the clock flowing from the new epoch boundary.
    *state.clock.lock().unwrap_or_else(|p| p.into_inner()) = ClockMode::Auto {
        anchor_ms: new_ts,
        anchor_at: std::time::Instant::now(),
    };
    Ok(Json(EpochResult {
        epoch,
        epoch_start_timestamp_ms: new_ts,
        clock_timestamp_ms: new_ts,
        note: "shallow advance: TxContext epoch/timestamp and the SuiSystemState (0x5) epoch move \
               forward; staking rewards, validator-set rotation and exchange-rate growth are not \
               settled"
            .to_string(),
    }))
}

#[derive(Deserialize)]
struct SetObject {
    object_id: String,
    contents_base64: String,
    bump_version: Option<bool>,
}

#[derive(Serialize)]
struct SetObjectResult {
    object_id: String,
    version: u64,
}

async fn object_set_contents(
    State(state): State<Arc<ForkState>>,
    Json(req): Json<SetObject>,
) -> Result<Json<SetObjectResult>, ApiError> {
    let id = ObjectID::from_hex_literal(&req.object_id).map_err(bad)?;
    let contents = b64_decode(&req.contents_base64).map_err(bad)?;
    let bump = req.bump_version.unwrap_or(true);
    let s = state.clone();
    let version =
        blocking(move || crate::cheats::set_object_contents(&s.fork, id, contents, bump)).await?;
    Ok(Json(SetObjectResult {
        object_id: id.to_hex_literal(),
        version,
    }))
}

#[derive(Serialize)]
struct SnapshotResult {
    id: u64,
}

/// Capture the fork's current state (overlay + epoch + clock) and return its id.
async fn snapshot(State(state): State<Arc<ForkState>>) -> Result<Json<SnapshotResult>, ApiError> {
    let s = state.clone();
    let id = tokio::task::spawn_blocking(move || s.take_snapshot())
        .await
        .map_err(ise)?;
    Ok(Json(SnapshotResult { id }))
}

#[derive(Deserialize)]
struct Revert {
    id: u64,
}

#[derive(Serialize)]
struct RevertResult {
    id: u64,
    epoch: u64,
    clock_timestamp_ms: u64,
    executed_transactions: u64,
}

/// Roll the fork back to a previously captured snapshot.
async fn revert(
    State(state): State<Arc<ForkState>>,
    Json(req): Json<Revert>,
) -> Result<Json<RevertResult>, ApiError> {
    let s = state.clone();
    let id = req.id;
    let clock_timestamp_ms = blocking(move || {
        s.revert(id)?;
        crate::cheats::clock_timestamp_ms(&s.fork)
    })
    .await?;
    Ok(Json(RevertResult {
        id: req.id,
        epoch: state.epoch(),
        clock_timestamp_ms,
        executed_transactions: state.fork.store().executed_count() as u64,
    }))
}

#[derive(Deserialize)]
struct StatePath {
    path: String,
}

#[derive(Serialize)]
struct StateResult {
    path: String,
    executed_transactions: u64,
    epoch: u64,
}

/// Write the fork's overlay + epoch + clock to a file on disk.
async fn state_dump(
    State(state): State<Arc<ForkState>>,
    Json(req): Json<StatePath>,
) -> Result<Json<StateResult>, ApiError> {
    let s = state.clone();
    let path = req.path.clone();
    blocking(move || s.dump_to(&path)).await?;
    Ok(Json(StateResult {
        path: req.path,
        executed_transactions: state.fork.store().executed_count() as u64,
        epoch: state.epoch(),
    }))
}

/// Reload a previously dumped fork state from disk (same chain + checkpoint).
async fn state_load(
    State(state): State<Arc<ForkState>>,
    Json(req): Json<StatePath>,
) -> Result<Json<StateResult>, ApiError> {
    let s = state.clone();
    let path = req.path.clone();
    blocking(move || s.load_from(&path)).await?;
    Ok(Json(StateResult {
        path: req.path,
        executed_transactions: state.fork.store().executed_count() as u64,
        epoch: state.epoch(),
    }))
}

#[derive(Deserialize)]
struct TraceReq {
    /// Base64 `TransactionData` BCS (the bytes `tx.build()` produces).
    transaction: String,
    /// Include the full compressed Move opcode trace (large). Default false.
    full: Option<bool>,
}

/// Dry-run a transaction and return an execution trace (command summary, gas,
/// status, object changes; optionally the full Move trace).
async fn trace(
    State(state): State<Arc<ForkState>>,
    Json(req): Json<TraceReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let s = state.clone();
    let full = req.full.unwrap_or(false);
    let tx = req.transaction;
    let report = blocking(move || crate::serve::trace_transaction(&s, &tx, full)).await?;
    Ok(Json(report))
}
