// Copyright (c) Inodra
// SPDX-License-Identifier: Apache-2.0

//! Aquarium CLI — fork Sui mainnet locally and inspect / mutate it.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use sui_types::base_types::ObjectID;

use aquarium::fork::MainnetFork;
use aquarium::gql::Gql;

#[derive(Parser)]
#[command(name = "aquarium", version, about = "Fork Sui mainnet locally.")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show fork metadata: chain id, latest checkpoint, fork point + epoch.
    Info {
        /// Checkpoint to fork from (defaults to the latest executed checkpoint).
        #[arg(long)]
        checkpoint: Option<u64>,
    },
    /// Read an object as the fork sees it (overlay first, else mainnet@fork).
    Object {
        /// Object id, e.g. 0x6 (the Clock) or a full 0x… address.
        #[arg(long)]
        id: String,
        /// Checkpoint to fork from (defaults to the latest executed checkpoint).
        #[arg(long)]
        checkpoint: Option<u64>,
    },
    /// Execute a minimal gas-only transaction against the fork to demonstrate
    /// that local execution mutates the overlay while mainnet is untouched.
    #[cfg(feature = "execute")]
    Demo {
        /// Address that owns the gas coin (and signs, conceptually).
        #[arg(long)]
        sender: String,
        /// A SUI coin object id owned by `sender`, used as gas.
        #[arg(long)]
        coin: String,
        /// Checkpoint to fork from (defaults to the latest executed checkpoint).
        #[arg(long)]
        checkpoint: Option<u64>,
        /// Number of transactions to execute in sequence against the fork. Each
        /// one consumes the previous one's output (chained local execution).
        #[arg(long, default_value_t = 1)]
        repeat: u32,
    },
    /// Serve the fork over a local sui.rpc.v2 gRPC endpoint so standard Sui
    /// tooling (grpcurl, SDKs) can read it and execute transactions against it.
    /// Signatures are NOT verified — any mainnet account can be impersonated
    /// locally. The real chain is never touched.
    #[cfg(feature = "serve")]
    Serve {
        /// Checkpoint to fork from (defaults to the latest executed checkpoint).
        #[arg(long)]
        checkpoint: Option<u64>,
        /// Port to listen on (binds 127.0.0.1).
        #[arg(long, default_value_t = 9123)]
        port: u16,
    },
}

fn resolve_checkpoint(gql: &Gql, requested: Option<u64>) -> Result<u64> {
    match requested {
        Some(cp) => Ok(cp),
        None => gql
            .latest_checkpoint()
            .context("resolving latest checkpoint"),
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let cli = Cli::parse();
    let gql = Gql::mainnet()?;

    match cli.command {
        Command::Info { checkpoint } => {
            let latest = gql.latest_checkpoint()?;
            // Reuse the value we already fetched when no checkpoint was pinned,
            // so latest and fork point are consistent (and we avoid a 2nd call).
            let cp = checkpoint.unwrap_or(latest);
            let chain = gql.chain_identifier()?;
            let epoch = gql.checkpoint_epoch(cp)?;
            println!("network          mainnet");
            println!("chain identifier {chain}");
            println!("latest checkpoint {latest}");
            println!("fork checkpoint  {cp}");
            println!("fork epoch       {epoch}");
        }
        Command::Object { id, checkpoint } => {
            let object_id = ObjectID::from_hex_literal(&id)
                .with_context(|| format!("parsing object id {id}"))?;
            let cp = resolve_checkpoint(&gql, checkpoint)?;
            let fork = MainnetFork::mainnet(cp)?;
            match fork.object(object_id)? {
                None => {
                    println!("object {object_id} not found at checkpoint {cp}");
                }
                Some(obj) => {
                    println!("object    {}", obj.id());
                    println!("version   {}", obj.version().value());
                    println!("digest    {}", obj.digest());
                    println!("owner     {:?}", obj.owner());
                    if obj.is_package() {
                        println!("kind      package");
                    } else if let Some(tag) = obj.struct_tag() {
                        println!("type      {tag}");
                    }
                    println!("(fork checkpoint {cp})");
                }
            }
        }
        #[cfg(feature = "execute")]
        Command::Demo {
            sender,
            coin,
            checkpoint,
            repeat,
        } => {
            demo(&gql, &sender, &coin, checkpoint, repeat)?;
        }
        #[cfg(feature = "serve")]
        Command::Serve { checkpoint, port } => {
            let cp = resolve_checkpoint(&gql, checkpoint)?;
            let chain_id = gql.chain_identifier()?;
            let epoch = gql.checkpoint_epoch(cp)?;
            let fork_digest = gql.checkpoint_digest(cp)?;
            let fork = MainnetFork::mainnet(cp)?;
            let vm = fork.vm()?;
            println!("aquarium gRPC fork of mainnet");
            println!("  chain id         {chain_id}");
            println!("  fork checkpoint  {cp}  (epoch {epoch})");
            println!("  reference gas    {} MIST", vm.reference_gas_price());
            println!("  listening on     127.0.0.1:{port}  (sui.rpc.v2, reflection enabled)");
            println!("\ntry:");
            println!(
                "  grpcurl -plaintext 127.0.0.1:{port} sui.rpc.v2.LedgerService.GetServiceInfo"
            );
            println!(
                "  grpcurl -plaintext -d '{{\"object_id\":\"0x6\"}}' 127.0.0.1:{port} sui.rpc.v2.LedgerService.GetObject"
            );
            aquarium::serve::run(fork, vm, chain_id, epoch, fork_digest, port)?;
        }
    }
    Ok(())
}

#[cfg(feature = "execute")]
fn demo(gql: &Gql, sender: &str, coin: &str, checkpoint: Option<u64>, repeat: u32) -> Result<()> {
    use std::str::FromStr;
    use sui_types::base_types::SuiAddress;
    use sui_types::gas_coin::GasCoin;
    use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
    use sui_types::transaction::TransactionData;

    // Min budget that covers mainnet's base transaction cost; below this even an
    // empty block is rejected as InsufficientGas regardless of coin balance.
    const MIN_BUDGET_MIST: u64 = 5_000_000;
    const MAX_BUDGET_MIST: u64 = 50_000_000;

    let sender: SuiAddress = SuiAddress::from_str(sender).context("parsing sender address")?;
    let coin_id = ObjectID::from_hex_literal(coin).context("parsing coin id")?;
    let repeat = repeat.max(1);
    let cp = resolve_checkpoint(gql, checkpoint)?;

    let fork = MainnetFork::mainnet(cp)?;
    let vm = fork.vm()?;
    let epoch = vm.epoch();
    let price = vm.reference_gas_price();

    let coin_obj = fork
        .object(coin_id)?
        .with_context(|| format!("gas coin {coin_id} not found at checkpoint {cp}"))?;
    let owner = coin_obj.owner().get_address_owner_address().ok();
    // The gas coin must actually be owned by the sender, or this transaction
    // could never execute on mainnet — keep the demo honest about parity.
    if owner != Some(sender) {
        anyhow::bail!(
            "gas coin {coin_id} is not address-owned by sender {sender} (owner: {owner:?})"
        );
    }
    // `GasCoin::try_from` only succeeds for a SUI coin (0x2::sui::SUI), so this
    // both validates the gas-coin type and reads its balance.
    let balance = GasCoin::try_from(&coin_obj)
        .map_err(|e| anyhow::anyhow!("gas object is not a SUI coin: {e}"))?
        .value();
    let before_version = coin_obj.version().value();

    if balance < MIN_BUDGET_MIST {
        anyhow::bail!(
            "gas coin balance {balance} MIST is below the minimum gas budget {MIN_BUDGET_MIST}"
        );
    }

    println!("fork checkpoint  {cp}  (epoch {epoch})");
    println!("gas coin         {coin_id}");
    println!("  owner          {owner:?}");
    println!("  balance        {balance} MIST");
    println!("  version before {before_version}");
    println!("\nexecuting {repeat} gas-only tx(s) in sequence (price {price})…");

    // Each iteration re-reads the gas coin *as the fork now sees it* (i.e. the
    // version produced by the previous local tx), proving the overlay feeds back
    // into execution — the chain continues locally.
    let mut last_digest = None;
    for i in 1..=repeat {
        let coin_now = fork
            .object(coin_id)?
            .context("gas coin missing from overlay between transactions")?;
        let gas_ref = coin_now.compute_object_reference();
        let coin_balance = GasCoin::try_from(&coin_now)
            .map_err(|e| anyhow::anyhow!("overlay gas object is not a SUI coin: {e}"))?
            .value();
        if coin_balance < MIN_BUDGET_MIST {
            anyhow::bail!(
                "gas coin depleted to {coin_balance} MIST after {} tx(s)",
                i - 1
            );
        }
        let budget = coin_balance.clamp(MIN_BUDGET_MIST, MAX_BUDGET_MIST);
        let pt = ProgrammableTransactionBuilder::new().finish();
        let tx = TransactionData::new_programmable(sender, vec![gas_ref], pt, budget, price);
        let outcome = fork.execute(&vm, tx)?;
        let status = match &outcome.status {
            Ok(()) => "SUCCESS".to_string(),
            Err(e) => format!("FAILED (gas still charged): {e}"),
        };
        println!(
            "  tx {i:>3}: v{} -> v{}  [{status}]",
            gas_ref.1.value(),
            outcome
                .written
                .iter()
                .find(|o| o.id() == coin_id)
                .map(|o| o.version().value())
                .unwrap_or_default(),
        );
        last_digest = Some(outcome.digest);
    }

    // The overlay now holds the bumped gas coin at a brand-new version that does
    // not exist on mainnet — that is the fork isolation we want to demonstrate.
    let after = fork
        .object(coin_id)?
        .context("coin vanished from overlay")?;
    let after_version = after.version().value();
    println!("\noverlay coin (locally produced after {repeat} tx(s)):");
    println!("  version  {after_version}  (mainnet fork point was {before_version})");
    println!("  digest   {}", after.digest());
    if let Some(d) = last_digest {
        println!("  last tx  {d}");
    }
    println!("\nFork isolation — verify against the independent public gRPC fullnode:");
    println!(
        "  • the local version {after_version} does NOT exist on mainnet (expect NotFound):\n      grpcurl -d '{{\"object_id\":\"{coin_id}\",\"version\":{after_version}}}' \\\n        fullnode.mainnet.sui.io:443 sui.rpc.v2.LedgerService.GetObject"
    );
    println!(
        "  • the fork-point version {before_version} matches mainnet (read parity):\n      grpcurl -d '{{\"object_id\":\"{coin_id}\",\"version\":{before_version}}}' \\\n        fullnode.mainnet.sui.io:443 sui.rpc.v2.LedgerService.GetObject"
    );
    Ok(())
}
