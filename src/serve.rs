// Copyright (c) Inodra
// SPDX-License-Identifier: Apache-2.0

//! The local gRPC façade (`aquarium serve`).
//!
//! Exposes a running [`Fork`](crate::fork::Fork) over the standard
//! `sui.rpc.v2` gRPC surface, so ordinary Sui tooling (`grpcurl`, the SDKs)
//! can talk to the fork exactly as it would talk to a node:
//!
//! - `LedgerService` — `GetServiceInfo`, `GetObject`, `BatchGetObjects`,
//!   `GetTransaction` (reads resolve overlay-first, then mainnet@fork).
//! - `StateService` — `GetBalance` (overlay coins + locally accrued address
//!   balance + best-effort mainnet coins) and `ListOwnedObjects` (overlay
//!   objects only — a fork cannot enumerate mainnet by owner at a checkpoint).
//! - `TransactionExecutionService` — `ExecuteTransaction` commits to the
//!   overlay, `SimulateTransaction` dry-runs. Signatures are **not** verified
//!   (the fork bypasses validation, like replay); never point production
//!   tooling at this endpoint expecting real authorization.
//!
//! Unlisted methods return `UNIMPLEMENTED`.

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use sui_data_store::stores::DataStore;
use sui_rpc::field::FieldMaskTree;
use sui_rpc::merge::Merge;
use sui_rpc::proto::sui::rpc::v2 as proto;
use sui_rpc::proto::sui::rpc::v2::ledger_service_server::{LedgerService, LedgerServiceServer};
use sui_rpc::proto::sui::rpc::v2::state_service_server::{StateService, StateServiceServer};
use sui_rpc::proto::sui::rpc::v2::transaction_execution_service_server::{
    TransactionExecutionService, TransactionExecutionServiceServer,
};
use sui_types::TypeTag;
use sui_types::base_types::{ObjectID, SuiAddress};
use sui_types::coin::Coin;
use sui_types::object::{Object, Owner};
use tonic::{Request, Response, Status};

use crate::engine::Vm;
use crate::fork::Fork;
use crate::gql::Gql;

/// Everything a handler needs, shared across services.
struct ForkState {
    fork: Fork<DataStore>,
    vm: Vm,
    chain_id: String,
    epoch: u64,
    /// Base58 digest of the mainnet checkpoint this fork branched from.
    fork_checkpoint_digest: String,
}

/// The gRPC service implementation (one clone per registered service).
#[derive(Clone)]
struct AquariumRpc(Arc<ForkState>);

/// Run the gRPC server on `127.0.0.1:port` until interrupted. Blocking.
pub fn run(
    fork: Fork<DataStore>,
    vm: Vm,
    chain_id: String,
    epoch: u64,
    fork_checkpoint_digest: String,
    port: u16,
) -> Result<()> {
    let state = AquariumRpc(Arc::new(ForkState {
        fork,
        vm,
        chain_id,
        epoch,
        fork_checkpoint_digest,
    }));

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
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;
    runtime.block_on(async move {
        tonic::transport::Server::builder()
            // Serve both native gRPC (HTTP/2) and gRPC-Web (HTTP/1.1) so both
            // grpcurl/SDK gRPC clients and browsers work against one port.
            .accept_http1(true)
            .layer(cors)
            .layer(tonic_web::GrpcWebLayer::new())
            .add_service(LedgerServiceServer::new(state.clone()))
            .add_service(StateServiceServer::new(state.clone()))
            .add_service(TransactionExecutionServiceServer::new(state))
            .add_service(reflection)
            .serve(addr)
            .await
            .context("gRPC server terminated")
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
        message.chain = Some("mainnet".to_string());
        message.epoch = Some(state.epoch);
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

        let all = FieldMaskTree::new_wildcard();
        let mut message = proto::ExecutedTransaction::default();
        message.digest = Some(digest);
        let sdk_tx: sui_sdk_types::Transaction = bcs_bridge(&info.data).map_err(internal)?;
        let mut transaction = proto::Transaction::default();
        transaction.merge(sdk_tx, &all);
        message.transaction = Some(transaction);
        let sdk_effects: sui_sdk_types::TransactionEffects =
            bcs_bridge(&info.effects).map_err(internal)?;
        let mut effects = proto::TransactionEffects::default();
        effects.merge(&sdk_effects, &all);
        message.effects = Some(effects);
        message.checkpoint = Some(info.checkpoint);

        let mut response = proto::GetTransactionResponse::default();
        response.transaction = Some(message);
        Ok(Response::new(response))
    }

    async fn batch_get_transactions(
        &self,
        _request: Request<proto::BatchGetTransactionsRequest>,
    ) -> Result<Response<proto::BatchGetTransactionsResponse>, Status> {
        Err(Status::unimplemented("BatchGetTransactions"))
    }

    async fn get_checkpoint(
        &self,
        request: Request<proto::GetCheckpointRequest>,
    ) -> Result<Response<proto::GetCheckpointResponse>, Status> {
        use proto::get_checkpoint_request::CheckpointId;
        use sui_types::transaction::TransactionDataAPI;
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
        summary.epoch = Some(state.epoch);
        summary.total_network_transactions = Some(total);

        let mut checkpoint = proto::Checkpoint::default();
        checkpoint.sequence_number = Some(fork_cp);
        checkpoint.digest = Some(state.fork_checkpoint_digest.clone());
        checkpoint.summary = Some(summary);
        checkpoint.transactions = transactions;

        let mut response = proto::GetCheckpointResponse::default();
        response.checkpoint = Some(checkpoint);
        Ok(Response::new(response))
    }

    async fn get_epoch(
        &self,
        _request: Request<proto::GetEpochRequest>,
    ) -> Result<Response<proto::GetEpochResponse>, Status> {
        let state = &self.0;
        let mut epoch = proto::Epoch::default();
        epoch.epoch = Some(state.epoch);
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
    let gql = Gql::mainnet()?;
    for (id, value) in gql.owned_coin_values(&owner.to_string(), &canonical)? {
        let id = ObjectID::from_hex_literal(&id).map_err(|e| anyhow!("bad coin id: {e}"))?;
        if !touched.contains(&id) {
            coin_balance += value as u128;
        }
    }

    let address_balance = store.address_balance(owner, &canonical);
    Ok((coin_balance, address_balance))
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

        let mut balance = proto::Balance::default();
        balance.coin_type = Some(coin_type.to_canonical_string(true));
        balance.coin_balance = Some(coin_balance.try_into().unwrap_or(u64::MAX));
        balance.address_balance = Some(address_balance.try_into().unwrap_or(u64::MAX));
        balance.balance = Some(
            (coin_balance + address_balance)
                .try_into()
                .unwrap_or(u64::MAX),
        );
        let mut response = proto::GetBalanceResponse::default();
        response.balance = Some(balance);
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
        _request: Request<proto::GetCoinInfoRequest>,
    ) -> Result<Response<proto::GetCoinInfoResponse>, Status> {
        Err(Status::unimplemented("GetCoinInfo"))
    }

    async fn list_balances(
        &self,
        _request: Request<proto::ListBalancesRequest>,
    ) -> Result<Response<proto::ListBalancesResponse>, Status> {
        Err(Status::unimplemented("ListBalances"))
    }

    async fn list_dynamic_fields(
        &self,
        _request: Request<proto::ListDynamicFieldsRequest>,
    ) -> Result<Response<proto::ListDynamicFieldsResponse>, Status> {
        Err(Status::unimplemented("ListDynamicFields"))
    }
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
        let outcome = tokio::task::spawn_blocking(move || state.fork.execute(&state.vm, tx_data))
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
