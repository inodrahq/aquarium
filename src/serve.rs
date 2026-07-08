// Copyright (c) Inodra
// SPDX-License-Identifier: Apache-2.0

//! The local gRPC façade (`aquarium serve`).
//!
//! Exposes a running [`Fork`](crate::fork::Fork) over the standard
//! `sui.rpc.v2` gRPC surface, so ordinary Sui tooling (`grpcurl`, the SDKs)
//! can talk to the fork exactly as it would talk to a node:
//!
//! - `LedgerService` — `GetServiceInfo`, `GetObject`, `BatchGetObjects`,
//!   `GetTransaction`, `BatchGetTransactions`, `GetCheckpoint`, `GetEpoch`.
//! - `StateService` — `GetBalance`, `ListBalances`, `ListOwnedObjects`,
//!   `GetCoinInfo`, `ListDynamicFields`.
//! - `MovePackageService` — `GetPackage`, `GetDatatype`, `GetFunction`,
//!   `ListPackageVersions` (parsed from on-fork bytecode).
//! - `TransactionExecutionService` — `ExecuteTransaction` / `SimulateTransaction`.
//!   Signatures are **not** verified (the fork bypasses validation, like replay).
//! - `SubscriptionService` — `SubscribeCheckpoints` over the fork's synthetic
//!   checkpoint feed.
//!
//! Alongside the gRPC endpoint the server exposes a small **cheat control** JSON
//! API (see [`crate::control`]) on a separate port: advance the clock, cross an
//! epoch boundary, override an object. Those are deliberately kept off the
//! `sui.rpc.v2` surface so it stays a faithful node twin.

use std::sync::Arc;

use crate::fork::CachedNetworkStore;
use anyhow::{Context, Result, anyhow};
use base64::Engine;
use sui_rpc::field::FieldMaskTree;
use sui_rpc::merge::Merge;
use sui_rpc::proto::sui::rpc::v2 as proto;
use sui_rpc::proto::sui::rpc::v2::ledger_service_server::{LedgerService, LedgerServiceServer};
use sui_rpc::proto::sui::rpc::v2::move_package_service_server::{
    MovePackageService, MovePackageServiceServer,
};
use sui_rpc::proto::sui::rpc::v2::state_service_server::{StateService, StateServiceServer};
use sui_rpc::proto::sui::rpc::v2::subscription_service_server::{
    SubscriptionService, SubscriptionServiceServer,
};
use sui_rpc::proto::sui::rpc::v2::transaction_execution_service_server::{
    TransactionExecutionService, TransactionExecutionServiceServer,
};
use sui_types::TypeTag;
use sui_types::base_types::{ObjectID, SuiAddress};
use sui_types::coin::{Coin, CoinMetadata, TreasuryCap};
use sui_types::object::{Object, Owner};
use tonic::{Request, Response, Status};

use crate::cheats::ClockMode;
use crate::engine::Vm;
use crate::fork::Fork;
use crate::gql::Gql;

/// A captured fork snapshot: the whole overlay plus the interior-mutable epoch
/// and clock state that live outside it. Cloneable, so a single snapshot can be
/// reverted to repeatedly.
#[derive(Clone)]
pub(crate) struct ForkSnapshot {
    overlay: crate::store::OverlayState,
    epoch: u64,
    epoch_start_timestamp_ms: u64,
    clock: ClockMode,
}

/// Everything a handler needs, shared across services and the control API.
pub(crate) struct ForkState {
    pub(crate) fork: Fork<CachedNetworkStore>,
    pub(crate) vm: Vm,
    pub(crate) chain_id: String,
    /// Human network name the fork is of (`mainnet` / `testnet` / a URL).
    pub(crate) chain_name: String,
    /// GraphQL endpoint for the forked network, used for the mainnet-side
    /// fallback reads (owned coins, coin metadata, balances, dynamic fields).
    pub(crate) network_gql_url: String,
    /// Base58 digest of the checkpoint this fork branched from.
    pub(crate) fork_checkpoint_digest: String,
    /// How the fork drives the on-chain `Clock` (`0x6`); see [`ClockMode`].
    pub(crate) clock: std::sync::Mutex<ClockMode>,
    /// Captured snapshots by id (see [`ForkState::take_snapshot`]).
    snapshots: std::sync::Mutex<std::collections::HashMap<u64, ForkSnapshot>>,
    /// Next snapshot id to hand out.
    next_snapshot: std::sync::atomic::AtomicU64,
}

impl ForkState {
    /// The fork's current epoch (may have been shallow-advanced via a cheat).
    pub(crate) fn epoch(&self) -> u64 {
        self.vm.epoch()
    }

    /// Capture the current fork state (overlay + epoch + clock) and return its
    /// snapshot id. The snapshot is independent of later mutations.
    pub(crate) fn take_snapshot(&self) -> u64 {
        let snapshot = ForkSnapshot {
            overlay: self.fork.store().snapshot(),
            epoch: self.vm.epoch(),
            epoch_start_timestamp_ms: self.vm.epoch_start_timestamp_ms(),
            clock: self.clock.lock().unwrap_or_else(|p| p.into_inner()).clone(),
        };
        let id = self
            .next_snapshot
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.snapshots
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id, snapshot);
        id
    }

    /// Roll the fork back to a captured snapshot. The snapshot is retained, so
    /// the same id can be reverted to again. Errors if the id is unknown.
    pub(crate) fn revert(&self, id: u64) -> Result<()> {
        let snapshot = self
            .snapshots
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&id)
            .cloned()
            .ok_or_else(|| anyhow!("no such snapshot {id}"))?;
        self.fork.store().restore(snapshot.overlay);
        self.vm
            .restore_epoch(snapshot.epoch, snapshot.epoch_start_timestamp_ms);
        *self.clock.lock().unwrap_or_else(|p| p.into_inner()) = snapshot.clock;
        Ok(())
    }

    /// Stamp the clock for the next transaction, per the active [`ClockMode`].
    /// Best-effort: a failure here (e.g. a transient read of `0x6`) must not
    /// abort the user's transaction, so it is logged and ignored.
    fn prepare_clock(&self) {
        let target = self
            .clock
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .target_ms();
        if let Some(ms) = target
            && let Err(e) = crate::cheats::set_clock_timestamp_ms(&self.fork, ms)
        {
            tracing::warn!("auto-clock update skipped: {e}");
        }
    }
}

/// The gRPC service implementation (one clone per registered service).
#[derive(Clone)]
struct AquariumRpc(Arc<ForkState>);

/// Run the gRPC server on `127.0.0.1:port` (and the JSON cheat-control API on
/// `127.0.0.1:control_port`) until interrupted. Blocking.
#[allow(clippy::too_many_arguments)]
pub fn run(
    fork: Fork<CachedNetworkStore>,
    vm: Vm,
    chain_id: String,
    chain_name: String,
    network_gql_url: String,
    fork_checkpoint_digest: String,
    port: u16,
    control_port: u16,
) -> Result<()> {
    // Default the clock to real wall-clock drift from the fork point, so a fresh
    // fork advances time like the live chain instead of freezing at the fork.
    let anchor_ms = crate::cheats::clock_timestamp_ms(&fork).unwrap_or(0);
    let shared = Arc::new(ForkState {
        fork,
        vm,
        chain_id,
        chain_name,
        network_gql_url,
        fork_checkpoint_digest,
        clock: std::sync::Mutex::new(ClockMode::Auto {
            anchor_ms,
            anchor_at: std::time::Instant::now(),
        }),
        snapshots: std::sync::Mutex::new(std::collections::HashMap::new()),
        next_snapshot: std::sync::atomic::AtomicU64::new(0),
    });
    let state = AquariumRpc(shared.clone());

    let reflection = tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(sui_rpc::proto::sui::rpc::v2::FILE_DESCRIPTOR_SET)
        .register_encoded_file_descriptor_set(sui_rpc::proto::google::protobuf::FILE_DESCRIPTOR_SET)
        .register_encoded_file_descriptor_set(sui_rpc::proto::google::rpc::FILE_DESCRIPTOR_SET)
        .build_v1()
        .context("build gRPC reflection service")?;

    // Permissive CORS so a browser on any localhost port (e.g. the explorer on
    // :3002) can call the fork; grpc-web tunnels gRPC over HTTP/1.1 fetch.
    let cors = tower_http::cors::CorsLayer::new()
        .allow_origin(tower_http::cors::Any)
        .allow_methods(tower_http::cors::Any)
        .allow_headers(tower_http::cors::Any)
        .expose_headers(tower_http::cors::Any);

    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let control_addr = std::net::SocketAddr::from(([127, 0, 0, 1], control_port));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;
    runtime.block_on(async move {
        let grpc = tonic::transport::Server::builder()
            // Serve both native gRPC (HTTP/2) and gRPC-Web (HTTP/1.1) so both
            // grpcurl/SDK gRPC clients and browsers work against one port.
            .accept_http1(true)
            .layer(cors)
            .layer(tonic_web::GrpcWebLayer::new())
            .add_service(LedgerServiceServer::new(state.clone()))
            .add_service(StateServiceServer::new(state.clone()))
            .add_service(MovePackageServiceServer::new(state.clone()))
            .add_service(SubscriptionServiceServer::new(state.clone()))
            .add_service(TransactionExecutionServiceServer::new(state))
            .add_service(reflection)
            .serve(addr);

        let control_router = crate::control::router(shared);
        let listener = tokio::net::TcpListener::bind(control_addr)
            .await
            .context("binding cheat-control port")?;
        let control = axum::serve(listener, control_router);

        tokio::try_join!(
            async { grpc.await.context("gRPC server terminated") },
            async { control.await.context("cheat-control server terminated") },
        )?;
        Ok(())
    })
}

// ---------- conversion helpers ----------

fn internal(e: impl std::fmt::Display) -> Status {
    Status::internal(e.to_string())
}

fn invalid(e: impl std::fmt::Display) -> Status {
    Status::invalid_argument(e.to_string())
}

/// Map an execution error to a status. Gas-validity failures (which the fork
/// checks itself, since it bypasses mainnet validation) become
/// `FailedPrecondition` with their message intact; anything else is `Internal`.
fn exec_error(e: anyhow::Error) -> Status {
    let msg = e.to_string();
    if msg.starts_with("InsufficientGas") || msg.starts_with("InvalidGasPrice") {
        Status::failed_precondition(msg)
    } else if msg.starts_with("InvalidInput") {
        Status::invalid_argument(msg)
    } else {
        Status::internal(msg)
    }
}

/// Bridge a `sui_types` value into its BCS-compatible `sui_sdk_types` twin.
fn bcs_bridge<A: serde::Serialize, B: serde::de::DeserializeOwned>(a: &A) -> Result<B> {
    Ok(bcs::from_bytes(&bcs::to_bytes(a)?)?)
}

fn object_to_proto(object: &Object, mask: &FieldMaskTree) -> Result<proto::Object> {
    let sdk: sui_sdk_types::Object = bcs_bridge(object)?;
    let mut message = proto::Object::default();
    message.merge(sdk, mask);
    Ok(message)
}

fn mask_or(mask: Option<prost_types::FieldMask>, default_paths: &[&str]) -> FieldMaskTree {
    match mask {
        Some(m) => FieldMaskTree::from(m),
        None => FieldMaskTree::from(prost_types::FieldMask {
            paths: default_paths.iter().map(|p| p.to_string()).collect(),
        }),
    }
}

fn parse_object_id(s: &str) -> Result<ObjectID, Status> {
    ObjectID::from_hex_literal(s).map_err(|e| invalid(format!("invalid object id {s}: {e}")))
}

fn parse_address(s: &str) -> Result<SuiAddress, Status> {
    use std::str::FromStr;
    SuiAddress::from_str(s).map_err(|e| invalid(format!("invalid address {s}: {e}")))
}

/// Decode the transaction from an `ExecuteTransaction`/`SimulateTransaction`
/// request into the native `TransactionData` the fork VM executes.
fn decode_transaction(
    transaction: Option<&proto::Transaction>,
) -> Result<sui_types::transaction::TransactionData, Status> {
    let proto_tx = transaction.ok_or_else(|| invalid("missing transaction"))?;
    let sdk_tx = sui_sdk_types::Transaction::try_from(proto_tx)
        .map_err(|e| invalid(format!("could not decode transaction: {e}")))?;
    bcs_bridge(&sdk_tx).map_err(|e| invalid(format!("transaction BCS bridge failed: {e}")))
}

/// Is `object` a coin of `coin_type` (canonical form) owned by `owner`?
fn coin_value_for(object: &Object, owner: SuiAddress, coin_type_canonical: &str) -> Option<u64> {
    let owned = match object.owner() {
        Owner::AddressOwner(a) => *a == owner,
        Owner::ConsensusAddressOwner { owner: a, .. } => *a == owner,
        _ => false,
    };
    if !owned {
        return None;
    }
    let (ty, value) = Coin::extract_balance_if_coin(object).ok().flatten()?;
    (ty.to_canonical_string(true) == coin_type_canonical).then_some(value)
}

/// Build the `ExecutedTransaction` message for stored transaction info.
fn executed_tx_to_proto(
    digest: &str,
    info: &sui_data_store::TransactionInfo,
) -> Result<proto::ExecutedTransaction> {
    let all = FieldMaskTree::new_wildcard();
    let mut message = proto::ExecutedTransaction::default();
    message.digest = Some(digest.to_string());
    let sdk_tx: sui_sdk_types::Transaction = bcs_bridge(&info.data)?;
    let mut transaction = proto::Transaction::default();
    transaction.merge(sdk_tx, &all);
    message.transaction = Some(transaction);
    let sdk_effects: sui_sdk_types::TransactionEffects = bcs_bridge(&info.effects)?;
    let mut effects = proto::TransactionEffects::default();
    effects.merge(&sdk_effects, &all);
    message.effects = Some(effects);
    message.checkpoint = Some(info.checkpoint);
    Ok(message)
}

/// Build the `ExecutedTransaction` message for an execution/simulation outcome.
fn outcome_to_proto(
    outcome: &crate::engine::ExecutionOutcome,
    fork_checkpoint: u64,
) -> Result<proto::ExecutedTransaction> {
    let all = FieldMaskTree::new_wildcard();
    let sdk_effects: sui_sdk_types::TransactionEffects = bcs_bridge(&outcome.effects)?;
    let mut effects = proto::TransactionEffects::default();
    effects.merge(&sdk_effects, &all);

    let sdk_tx: sui_sdk_types::Transaction = bcs_bridge(&outcome.tx_data)?;
    let mut transaction = proto::Transaction::default();
    transaction.merge(sdk_tx, &all);

    let mut message = proto::ExecutedTransaction::default();
    message.digest = Some(outcome.digest.to_string());
    message.transaction = Some(transaction);
    message.effects = Some(effects);
    message.checkpoint = Some(fork_checkpoint);
    Ok(message)
}

/// Build the fork's synthetic checkpoint: its branch height + real digest + the
/// most recently executed local transactions. Shared by `GetCheckpoint` and the
/// subscription feed.
fn synthetic_checkpoint(state: &ForkState) -> proto::Checkpoint {
    use sui_types::transaction::TransactionDataAPI;
    let fork_cp = state.fork.fork_checkpoint();
    let recent = state.fork.store().recent_executed(20);
    let total = fork_cp + state.fork.store().executed_count() as u64;

    let transactions: Vec<proto::ExecutedTransaction> = recent
        .into_iter()
        .map(|(digest, info)| {
            let mut tx = proto::Transaction::default();
            tx.sender = Some(info.data.sender().to_string());
            let mut exec = proto::ExecutedTransaction::default();
            exec.digest = Some(digest);
            exec.transaction = Some(tx);
            exec.checkpoint = Some(fork_cp);
            exec
        })
        .collect();

    let mut summary = proto::CheckpointSummary::default();
    summary.sequence_number = Some(fork_cp);
    summary.digest = Some(state.fork_checkpoint_digest.clone());
    summary.epoch = Some(state.epoch());
    summary.total_network_transactions = Some(total);

    let mut checkpoint = proto::Checkpoint::default();
    checkpoint.sequence_number = Some(fork_cp);
    checkpoint.digest = Some(state.fork_checkpoint_digest.clone());
    checkpoint.summary = Some(summary);
    checkpoint.transactions = transactions;
    checkpoint
}

// ---------- LedgerService ----------

#[tonic::async_trait]
impl LedgerService for AquariumRpc {
    async fn get_service_info(
        &self,
        _request: Request<proto::GetServiceInfoRequest>,
    ) -> Result<Response<proto::GetServiceInfoResponse>, Status> {
        let state = &self.0;
        let mut message = proto::GetServiceInfoResponse::default();
        message.chain_id = Some(state.chain_id.clone());
        message.chain = Some(state.chain_name.clone());
        message.epoch = Some(state.epoch());
        message.checkpoint_height = Some(state.fork.fork_checkpoint());
        message.server = Some(concat!("aquarium/", env!("CARGO_PKG_VERSION")).to_string());
        Ok(Response::new(message))
    }

    async fn get_object(
        &self,
        request: Request<proto::GetObjectRequest>,
    ) -> Result<Response<proto::GetObjectResponse>, Status> {
        let req = request.into_inner();
        let id = parse_object_id(req.object_id.as_deref().unwrap_or_default())?;
        let version = req.version;
        let mask = mask_or(req.read_mask, &["object_id", "version", "digest"]);

        let state = self.0.clone();
        let object = tokio::task::spawn_blocking(move || match version {
            Some(v) => state.fork.object_at_version(id, v),
            None => state.fork.object(id),
        })
        .await
        .map_err(internal)?
        .map_err(internal)?;

        let object = object.ok_or_else(|| Status::not_found(format!("object {id} not found")))?;
        let mut response = proto::GetObjectResponse::default();
        response.object = Some(object_to_proto(&object, &mask).map_err(internal)?);
        Ok(Response::new(response))
    }

    async fn batch_get_objects(
        &self,
        request: Request<proto::BatchGetObjectsRequest>,
    ) -> Result<Response<proto::BatchGetObjectsResponse>, Status> {
        let req = request.into_inner();
        let mask = mask_or(req.read_mask, &["object_id", "version", "digest"]);
        let mut results = Vec::with_capacity(req.requests.len());
        for r in req.requests {
            let id = parse_object_id(r.object_id.as_deref().unwrap_or_default())?;
            let version = r.version;
            let state = self.0.clone();
            let object = tokio::task::spawn_blocking(move || match version {
                Some(v) => state.fork.object_at_version(id, v),
                None => state.fork.object(id),
            })
            .await
            .map_err(internal)?
            .map_err(internal)?;
            let mut result = proto::GetObjectResult::default();
            result.result = Some(match object {
                Some(o) => proto::get_object_result::Result::Object(
                    object_to_proto(&o, &mask).map_err(internal)?,
                ),
                None => {
                    proto::get_object_result::Result::Error(sui_rpc::proto::google::rpc::Status {
                        code: tonic::Code::NotFound as i32,
                        message: format!("object {id} not found"),
                        details: vec![],
                    })
                }
            });
            results.push(result);
        }
        let mut response = proto::BatchGetObjectsResponse::default();
        response.objects = results;
        Ok(Response::new(response))
    }

    async fn get_transaction(
        &self,
        request: Request<proto::GetTransactionRequest>,
    ) -> Result<Response<proto::GetTransactionResponse>, Status> {
        use sui_data_store::TransactionStore;
        let req = request.into_inner();
        let digest = req.digest.clone().unwrap_or_default();
        let state = self.0.clone();
        let info = {
            let digest = digest.clone();
            tokio::task::spawn_blocking(move || {
                state.fork.store().transaction_data_and_effects(&digest)
            })
            .await
            .map_err(internal)?
            .map_err(internal)?
        };
        let info =
            info.ok_or_else(|| Status::not_found(format!("transaction {digest} not found")))?;

        let mut response = proto::GetTransactionResponse::default();
        response.transaction = Some(executed_tx_to_proto(&digest, &info).map_err(internal)?);
        Ok(Response::new(response))
    }

    async fn batch_get_transactions(
        &self,
        request: Request<proto::BatchGetTransactionsRequest>,
    ) -> Result<Response<proto::BatchGetTransactionsResponse>, Status> {
        use sui_data_store::TransactionStore;
        let req = request.into_inner();
        let mut out = Vec::with_capacity(req.digests.len());
        for digest in req.digests {
            let state = self.0.clone();
            let info = {
                let digest = digest.clone();
                tokio::task::spawn_blocking(move || {
                    state.fork.store().transaction_data_and_effects(&digest)
                })
                .await
                .map_err(internal)?
                .map_err(internal)?
            };
            let mut result = proto::GetTransactionResult::default();
            result.result = Some(match info {
                Some(info) => proto::get_transaction_result::Result::Transaction(
                    executed_tx_to_proto(&digest, &info).map_err(internal)?,
                ),
                None => proto::get_transaction_result::Result::Error(
                    sui_rpc::proto::google::rpc::Status {
                        code: tonic::Code::NotFound as i32,
                        message: format!("transaction {digest} not found"),
                        details: vec![],
                    },
                ),
            });
            out.push(result);
        }
        let mut response = proto::BatchGetTransactionsResponse::default();
        response.transactions = out;
        Ok(Response::new(response))
    }

    async fn get_checkpoint(
        &self,
        request: Request<proto::GetCheckpointRequest>,
    ) -> Result<Response<proto::GetCheckpointResponse>, Status> {
        use proto::get_checkpoint_request::CheckpointId;
        let state = &self.0;
        let fork_cp = state.fork.fork_checkpoint();
        // A fork has exactly one "checkpoint": its branch point (which then
        // accretes locally executed transactions). Only that height, or "latest"
        // (no id), is answerable — any other sequence never existed on the fork.
        if let Some(CheckpointId::SequenceNumber(seq)) = request.into_inner().checkpoint_id
            && seq != fork_cp
        {
            return Err(Status::not_found(format!(
                "fork only has checkpoint {fork_cp}; {seq} was never executed locally"
            )));
        }

        let mut response = proto::GetCheckpointResponse::default();
        response.checkpoint = Some(synthetic_checkpoint(state));
        Ok(Response::new(response))
    }

    async fn get_epoch(
        &self,
        _request: Request<proto::GetEpochRequest>,
    ) -> Result<Response<proto::GetEpochResponse>, Status> {
        let state = &self.0;
        let mut epoch = proto::Epoch::default();
        epoch.epoch = Some(state.epoch());
        epoch.reference_gas_price = Some(state.vm.reference_gas_price());
        let mut response = proto::GetEpochResponse::default();
        response.epoch = Some(epoch);
        Ok(Response::new(response))
    }
}

// ---------- StateService ----------

/// Compute `(coin_balance, address_balance)` for `owner`/`coin_type` as the
/// fork sees it: coins in the overlay, plus mainnet coins the overlay has no
/// opinion about (best-effort live GraphQL enumeration — exact for fork-only
/// accounts, which have no mainnet history), plus locally accrued address
/// balance.
fn compute_balance(
    state: &ForkState,
    owner: SuiAddress,
    coin_type: &TypeTag,
) -> Result<(u128, u128)> {
    let canonical = coin_type.to_canonical_string(true);
    let store = state.fork.store();

    let mut coin_balance: u128 = 0;
    for object in store.overlay_objects() {
        if let Some(value) = coin_value_for(&object, owner, &canonical) {
            coin_balance += value as u128;
        }
    }

    // Mainnet-side coins (skipping any id the overlay already accounts for).
    let touched = store.overlay_touched_ids();
    let gql = Gql::new(&state.network_gql_url)?;
    for (id, value) in gql.owned_coin_values(&owner.to_string(), &canonical)? {
        let id = ObjectID::from_hex_literal(&id).map_err(|e| anyhow!("bad coin id: {e}"))?;
        if !touched.contains(&id) {
            coin_balance += value as u128;
        }
    }

    let address_balance = store.address_balance(owner, &canonical);
    Ok((coin_balance, address_balance))
}

fn balance_proto(coin_type: &str, coin_balance: u128, address_balance: u128) -> proto::Balance {
    let mut balance = proto::Balance::default();
    balance.coin_type = Some(coin_type.to_string());
    balance.coin_balance = Some(coin_balance.try_into().unwrap_or(u64::MAX));
    balance.address_balance = Some(address_balance.try_into().unwrap_or(u64::MAX));
    balance.balance = Some(
        (coin_balance + address_balance)
            .try_into()
            .unwrap_or(u64::MAX),
    );
    balance
}

#[tonic::async_trait]
impl StateService for AquariumRpc {
    async fn get_balance(
        &self,
        request: Request<proto::GetBalanceRequest>,
    ) -> Result<Response<proto::GetBalanceResponse>, Status> {
        let req = request.into_inner();
        let owner = parse_address(req.owner.as_deref().unwrap_or_default())?;
        let coin_type = sui_types::parse_sui_type_tag(req.coin_type.as_deref().unwrap_or_default())
            .map_err(|e| invalid(format!("invalid coin_type: {e}")))?;

        let state = self.0.clone();
        let (coin_balance, address_balance) = {
            let coin_type = coin_type.clone();
            tokio::task::spawn_blocking(move || compute_balance(&state, owner, &coin_type))
                .await
                .map_err(internal)?
                .map_err(internal)?
        };

        let mut response = proto::GetBalanceResponse::default();
        response.balance = Some(balance_proto(
            &coin_type.to_canonical_string(true),
            coin_balance,
            address_balance,
        ));
        Ok(Response::new(response))
    }

    async fn list_balances(
        &self,
        request: Request<proto::ListBalancesRequest>,
    ) -> Result<Response<proto::ListBalancesResponse>, Status> {
        let req = request.into_inner();
        let owner = parse_address(req.owner.as_deref().unwrap_or_default())?;

        let state = self.0.clone();
        let balances = tokio::task::spawn_blocking(move || -> Result<Vec<proto::Balance>> {
            let store = state.fork.store();
            // Discover every coin type the owner touches: overlay coins, overlay
            // address balances, and live mainnet balances.
            let mut types: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
            for object in store.overlay_objects() {
                if let Ok(Some((ty, _))) = Coin::extract_balance_if_coin(&object) {
                    let owned = matches!(object.owner(),
                        Owner::AddressOwner(a) | Owner::ConsensusAddressOwner { owner: a, .. } if *a == owner);
                    if owned {
                        types.insert(ty.to_canonical_string(true));
                    }
                }
            }
            for (ty, _) in store.address_balances_of(owner) {
                types.insert(ty);
            }
            if let Ok(gql) = Gql::new(&state.network_gql_url)
                && let Ok(network) = gql.address_balances(&owner.to_string())
            {
                for (ty, _) in network {
                    types.insert(ty);
                }
            }

            let mut out = Vec::new();
            for ty in types {
                let tag = match sui_types::parse_sui_type_tag(&ty) {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                let (coin_balance, address_balance) = compute_balance(&state, owner, &tag)?;
                if coin_balance == 0 && address_balance == 0 {
                    continue;
                }
                out.push(balance_proto(&ty, coin_balance, address_balance));
            }
            Ok(out)
        })
        .await
        .map_err(internal)?
        .map_err(internal)?;

        let mut response = proto::ListBalancesResponse::default();
        response.balances = balances;
        Ok(Response::new(response))
    }

    async fn list_owned_objects(
        &self,
        request: Request<proto::ListOwnedObjectsRequest>,
    ) -> Result<Response<proto::ListOwnedObjectsResponse>, Status> {
        let req = request.into_inner();
        let owner = parse_address(req.owner.as_deref().unwrap_or_default())?;
        let mask = mask_or(req.read_mask, &["object_id", "version", "object_type"]);

        // Overlay-only: a checkpoint-pinned fork cannot enumerate mainnet
        // objects by owner. Fork-created accounts are fully covered.
        let type_filter = req.object_type.clone();
        let state = self.0.clone();
        let objects = tokio::task::spawn_blocking(move || state.fork.store().overlay_objects())
            .await
            .map_err(internal)?;

        let mut out = Vec::new();
        for object in objects {
            let owned = match object.owner() {
                Owner::AddressOwner(a) => *a == owner,
                Owner::ConsensusAddressOwner { owner: a, .. } => *a == owner,
                _ => false,
            };
            if !owned {
                continue;
            }
            if let Some(filter) = type_filter.as_deref()
                && !type_matches(&object, filter).map_err(invalid)?
            {
                continue;
            }
            out.push(object_to_proto(&object, &mask).map_err(internal)?);
        }
        let mut response = proto::ListOwnedObjectsResponse::default();
        response.objects = out;
        Ok(Response::new(response))
    }

    async fn get_coin_info(
        &self,
        request: Request<proto::GetCoinInfoRequest>,
    ) -> Result<Response<proto::GetCoinInfoResponse>, Status> {
        let req = request.into_inner();
        let coin_type_str = req.coin_type.clone().unwrap_or_default();
        let coin_tag = sui_types::parse_sui_type_tag(&coin_type_str)
            .map_err(|e| invalid(format!("invalid coin_type: {e}")))?;
        let canonical = coin_tag.to_canonical_string(true);

        let state = self.0.clone();
        let response =
            tokio::task::spawn_blocking(move || -> Result<proto::GetCoinInfoResponse> {
                coin_info(&state, &coin_tag, &canonical)
            })
            .await
            .map_err(internal)?
            .map_err(internal)?;
        Ok(Response::new(response))
    }

    async fn list_dynamic_fields(
        &self,
        request: Request<proto::ListDynamicFieldsRequest>,
    ) -> Result<Response<proto::ListDynamicFieldsResponse>, Status> {
        let req = request.into_inner();
        let parent = parse_object_id(req.parent.as_deref().unwrap_or_default())?;
        let limit = req.page_size.unwrap_or(50).clamp(1, 1000) as usize;

        let state = self.0.clone();
        let fields = tokio::task::spawn_blocking(move || -> Result<Vec<proto::DynamicField>> {
            dynamic_fields(&state, parent, limit)
        })
        .await
        .map_err(internal)?
        .map_err(internal)?;

        let mut response = proto::ListDynamicFieldsResponse::default();
        response.dynamic_fields = fields;
        Ok(Response::new(response))
    }
}

/// Answer `GetCoinInfo`: coin metadata + treasury supply, overlay-first (for
/// coins minted on the fork) then live mainnet GraphQL.
fn coin_info(
    state: &ForkState,
    coin_tag: &TypeTag,
    canonical: &str,
) -> Result<proto::GetCoinInfoResponse> {
    let mut response = proto::GetCoinInfoResponse::default();
    response.coin_type = Some(canonical.to_string());

    // Overlay: a coin published on the fork has its CoinMetadata / TreasuryCap
    // in the overlay, invisible to mainnet GraphQL.
    for object in state.fork.store().overlay_objects() {
        let Some(tag) = object.struct_tag() else {
            continue;
        };
        if let Some(inner) = CoinMetadata::is_coin_metadata_with_coin_type(&tag)
            && TypeTag::Struct(Box::new(inner.clone())).to_canonical_string(true) == canonical
            && let Some(move_obj) = object.data.try_as_move()
            && let Ok(meta) = CoinMetadata::from_bcs_bytes(move_obj.contents())
        {
            let mut m = proto::CoinMetadata::default();
            m.id = Some(object.id().to_hex_literal());
            m.decimals = Some(meta.decimals as u32);
            m.name = Some(meta.name);
            m.symbol = Some(meta.symbol);
            m.description = Some(meta.description);
            m.icon_url = meta.icon_url;
            response.metadata = Some(m);
        }
        if let Some(inner) = TreasuryCap::is_treasury_with_coin_type(&tag)
            && TypeTag::Struct(Box::new(inner.clone())).to_canonical_string(true) == canonical
            && let Some(move_obj) = object.data.try_as_move()
            && let Ok(cap) = TreasuryCap::from_bcs_bytes(move_obj.contents())
        {
            let mut t = proto::CoinTreasury::default();
            t.id = Some(object.id().to_hex_literal());
            t.total_supply = Some(cap.total_supply.value);
            response.treasury = Some(t);
        }
    }

    // Fall back to live mainnet for anything the overlay didn't supply.
    if response.metadata.is_none() || response.treasury.is_none() {
        let gql = Gql::new(&state.network_gql_url)?;
        if let Some(meta) = gql.coin_metadata(canonical)? {
            if response.metadata.is_none() {
                let mut m = proto::CoinMetadata::default();
                m.decimals = Some(meta.decimals);
                m.name = Some(meta.name);
                m.symbol = Some(meta.symbol);
                m.description = Some(meta.description);
                m.icon_url = Some(meta.icon_url);
                response.metadata = Some(m);
            }
            if response.treasury.is_none()
                && let Some(supply) = meta.supply
            {
                let mut t = proto::CoinTreasury::default();
                t.total_supply = Some(supply.try_into().unwrap_or(u64::MAX));
                response.treasury = Some(t);
            }
        }
    }
    let _ = coin_tag;
    Ok(response)
}

/// Answer `ListDynamicFields`: overlay field wrappers owned by `parent`, then
/// live mainnet fields (with `field_id` derived from the name).
fn dynamic_fields(
    state: &ForkState,
    parent: ObjectID,
    limit: usize,
) -> Result<Vec<proto::DynamicField>> {
    use proto::dynamic_field::DynamicFieldKind;
    let parent_addr: SuiAddress = parent.into();
    let mut out = Vec::new();
    let mut seen: std::collections::BTreeSet<ObjectID> = std::collections::BTreeSet::new();

    // Overlay: locally created dynamic fields are Field<Name,Value> objects
    // owned by the parent's UID.
    for object in state.fork.store().overlay_objects() {
        let owned_by_parent = matches!(object.owner(), Owner::ObjectOwner(a) if *a == parent_addr);
        if !owned_by_parent {
            continue;
        }
        let mut df = proto::DynamicField::default();
        df.kind = Some(DynamicFieldKind::Field as i32);
        df.parent = Some(parent.to_hex_literal());
        df.field_id = Some(object.id().to_hex_literal());
        if let Some(tag) = object.struct_tag() {
            df.value_type = Some(TypeTag::Struct(Box::new(tag)).to_canonical_string(true));
        }
        seen.insert(object.id());
        out.push(df);
        if out.len() >= limit {
            return Ok(out);
        }
    }

    // Live mainnet fields (best-effort; derive the field id from the name).
    let gql = Gql::new(&state.network_gql_url)?;
    for f in gql.dynamic_fields(&parent.to_string(), limit)? {
        let mut df = proto::DynamicField::default();
        df.kind = Some(if f.is_object {
            DynamicFieldKind::Object as i32
        } else {
            DynamicFieldKind::Field as i32
        });
        df.parent = Some(parent.to_hex_literal());
        if let Ok(name_tag) = sui_types::parse_sui_type_tag(&f.name_type)
            && let Ok(field_id) =
                sui_types::dynamic_field::derive_dynamic_field_id(parent, &name_tag, &f.name_bcs)
        {
            if seen.contains(&field_id) {
                continue;
            }
            df.field_id = Some(field_id.to_hex_literal());
        }
        df.name = Some(f.name_bcs.into());
        df.value_type = Some(f.value_type);
        df.child_id = f.child_id;
        out.push(df);
        if out.len() >= limit {
            break;
        }
    }
    Ok(out)
}

/// Match an object's struct tag against a `ListOwnedObjects` type filter:
/// a filter without type params (`0x2::coin::Coin`) matches any instantiation;
/// a filter with params must match exactly.
fn type_matches(object: &Object, filter: &str) -> Result<bool, String> {
    let Some(tag) = object.struct_tag() else {
        return Ok(false);
    };
    if filter.contains('<') {
        let want = sui_types::parse_sui_type_tag(filter)
            .map_err(|e| format!("invalid object_type filter: {e}"))?;
        Ok(TypeTag::Struct(Box::new(tag)).to_canonical_string(true)
            == want.to_canonical_string(true))
    } else {
        let want = sui_types::parse_sui_struct_tag(filter)
            .map_err(|e| format!("invalid object_type filter: {e}"))?;
        Ok(tag.address == want.address && tag.module == want.module && tag.name == want.name)
    }
}

// ---------- MovePackageService ----------

#[tonic::async_trait]
impl MovePackageService for AquariumRpc {
    async fn get_package(
        &self,
        request: Request<proto::GetPackageRequest>,
    ) -> Result<Response<proto::GetPackageResponse>, Status> {
        let req = request.into_inner();
        let id = parse_object_id(req.package_id.as_deref().unwrap_or_default())?;
        let state = self.0.clone();
        let package = tokio::task::spawn_blocking(move || -> Result<proto::Package> {
            let object = state
                .fork
                .object(id)?
                .with_context(|| format!("package {id} not found on the fork"))?;
            let pkg = object
                .data
                .try_as_package()
                .with_context(|| format!("object {id} is not a Move package"))?;
            crate::movedesc::build_package(pkg)
        })
        .await
        .map_err(internal)?
        .map_err(internal)?;

        let mut response = proto::GetPackageResponse::default();
        response.package = Some(package);
        Ok(Response::new(response))
    }

    async fn get_datatype(
        &self,
        request: Request<proto::GetDatatypeRequest>,
    ) -> Result<Response<proto::GetDatatypeResponse>, Status> {
        let req = request.into_inner();
        let id = parse_object_id(req.package_id.as_deref().unwrap_or_default())?;
        let module = req.module_name.clone().unwrap_or_default();
        let name = req.name.clone().unwrap_or_default();
        let state = self.0.clone();
        let datatype =
            tokio::task::spawn_blocking(move || -> Result<Option<proto::DatatypeDescriptor>> {
                let object = state
                    .fork
                    .object(id)?
                    .with_context(|| format!("package {id} not found on the fork"))?;
                let pkg = object
                    .data
                    .try_as_package()
                    .with_context(|| format!("object {id} is not a Move package"))?;
                crate::movedesc::find_datatype(pkg, &module, &name)
            })
            .await
            .map_err(internal)?
            .map_err(internal)?;

        let datatype = datatype.ok_or_else(|| Status::not_found("datatype not found"))?;
        let mut response = proto::GetDatatypeResponse::default();
        response.datatype = Some(datatype);
        Ok(Response::new(response))
    }

    async fn get_function(
        &self,
        request: Request<proto::GetFunctionRequest>,
    ) -> Result<Response<proto::GetFunctionResponse>, Status> {
        let req = request.into_inner();
        let id = parse_object_id(req.package_id.as_deref().unwrap_or_default())?;
        let module = req.module_name.clone().unwrap_or_default();
        let name = req.name.clone().unwrap_or_default();
        let state = self.0.clone();
        let function =
            tokio::task::spawn_blocking(move || -> Result<Option<proto::FunctionDescriptor>> {
                let object = state
                    .fork
                    .object(id)?
                    .with_context(|| format!("package {id} not found on the fork"))?;
                let pkg = object
                    .data
                    .try_as_package()
                    .with_context(|| format!("object {id} is not a Move package"))?;
                crate::movedesc::find_function(pkg, &module, &name)
            })
            .await
            .map_err(internal)?
            .map_err(internal)?;

        let function = function.ok_or_else(|| Status::not_found("function not found"))?;
        let mut response = proto::GetFunctionResponse::default();
        response.function = Some(function);
        Ok(Response::new(response))
    }

    async fn list_package_versions(
        &self,
        request: Request<proto::ListPackageVersionsRequest>,
    ) -> Result<Response<proto::ListPackageVersionsResponse>, Status> {
        // A fork is pinned to one checkpoint, so it sees a single version of a
        // package. Return that one; a full version history would need a node's
        // package index, which a fork does not maintain.
        let req = request.into_inner();
        let id = parse_object_id(req.package_id.as_deref().unwrap_or_default())?;
        let state = self.0.clone();
        let version = tokio::task::spawn_blocking(move || -> Result<u64> {
            let object = state
                .fork
                .object(id)?
                .with_context(|| format!("package {id} not found on the fork"))?;
            Ok(object.version().value())
        })
        .await
        .map_err(internal)?
        .map_err(internal)?;

        let mut entry = proto::PackageVersion::default();
        entry.package_id = Some(id.to_hex_literal());
        entry.version = Some(version);
        let mut response = proto::ListPackageVersionsResponse::default();
        response.versions = vec![entry];
        Ok(Response::new(response))
    }
}

// ---------- SubscriptionService ----------

#[tonic::async_trait]
impl SubscriptionService for AquariumRpc {
    async fn subscribe_checkpoints(
        &self,
        _request: Request<proto::SubscribeCheckpointsRequest>,
    ) -> Result<Response<tonic::codegen::BoxStream<proto::SubscribeCheckpointsResponse>>, Status>
    {
        let state = self.0.clone();
        let (tx, rx) = tokio::sync::mpsc::channel(16);

        // A fork presents one checkpoint height that accretes locally executed
        // transactions; emit a response whenever the executed-tx count grows,
        // using `fork_checkpoint + executed_count` as a monotonic cursor.
        tokio::spawn(async move {
            let fork_cp = state.fork.fork_checkpoint();
            let mut last_cursor = u64::MAX;
            loop {
                let count = state.fork.store().executed_count() as u64;
                let cursor = fork_cp + count;
                if cursor != last_cursor {
                    last_cursor = cursor;
                    let mut resp = proto::SubscribeCheckpointsResponse::default();
                    resp.cursor = Some(cursor);
                    resp.checkpoint = Some(synthetic_checkpoint(&state));
                    if tx.send(Ok(resp)).await.is_err() {
                        break; // client hung up
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        });

        Ok(Response::new(Box::pin(
            tokio_stream::wrappers::ReceiverStream::new(rx),
        )))
    }
}

// ---------- TransactionExecutionService ----------

#[tonic::async_trait]
impl TransactionExecutionService for AquariumRpc {
    async fn execute_transaction(
        &self,
        request: Request<proto::ExecuteTransactionRequest>,
    ) -> Result<Response<proto::ExecuteTransactionResponse>, Status> {
        let req = request.into_inner();
        // Signatures are intentionally ignored: the fork bypasses validation
        // (replay-style), which is what lets you act as any mainnet account.
        let tx_data = decode_transaction(req.transaction.as_ref())?;

        let state = self.0.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            // Advance the clock (per the active mode) so the transaction sees
            // current time, then execute.
            state.prepare_clock();
            state.fork.execute(&state.vm, tx_data)
        })
        .await
        .map_err(internal)?
        .map_err(exec_error)?;

        let mut response = proto::ExecuteTransactionResponse::default();
        response.transaction =
            Some(outcome_to_proto(&outcome, self.0.fork.fork_checkpoint()).map_err(internal)?);
        Ok(Response::new(response))
    }

    async fn simulate_transaction(
        &self,
        request: Request<proto::SimulateTransactionRequest>,
    ) -> Result<Response<proto::SimulateTransactionResponse>, Status> {
        let req = request.into_inner();
        let tx_data = decode_transaction(req.transaction.as_ref())?;

        let state = self.0.clone();
        let outcome = tokio::task::spawn_blocking(move || state.fork.simulate(&state.vm, tx_data))
            .await
            .map_err(internal)?
            .map_err(exec_error)?;

        let mut response = proto::SimulateTransactionResponse::default();
        response.transaction =
            Some(outcome_to_proto(&outcome, self.0.fork.fork_checkpoint()).map_err(internal)?);
        Ok(Response::new(response))
    }
}

/// Base64-decode helper shared with the control API.
pub(crate) fn b64_decode(s: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(s.trim())
        .context("invalid base64")
}
