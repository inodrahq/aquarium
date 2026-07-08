# Aquarium — design

> Fork Sui mainnet locally. A contained, observable slice of the chain you can
> poke at without touching the real ocean.

Aquarium is to Sui what `anvil --fork-url` is to EVM: it lets you take **live
mainnet object state** and execute **new transactions** against it locally,
with no validators, no consensus, and no multi-terabyte snapshot. State is
fetched **lazily on demand** and cached; transactions you run mutate a local
**overlay** while the real chain is untouched.

## Why this is possible on Sui (the short version)

Two halves of "fork the chain and keep going":

- **State access** — *feasible and cheap.* Sui transactions declare their input
  objects explicitly, and the Move VM is deterministic given (inputs, protocol
  config). So we can fetch exactly the objects a transaction touches, execute
  locally, and cache. Mysten's `sui-replay-2` already does this for a *single
  historical* transaction; Aquarium generalizes it to a *continuous, writable*
  fork that accepts *new* transactions.
- **Consensus** — *skipped.* Continuing the real chain would mean replacing
  mainnet's stake-weighted validator committee with a local one (an invasive
  regenesis). Aquarium doesn't try. Like Anvil, it is its own trivial
  sequencer: it executes transactions serially against the overlay. You lose
  consensus timing and validator economics; you keep faithful Move execution
  against real state. For a dev tool that's the right trade.

## Architecture

```
                 ┌──────────────────────────────────────────────┐
                 │                  aquarium                     │
                 │                                              │
   new tx ──────▶│  Sequencer ──▶ Vm (sui-execution Executor)   │
                 │      │              │                         │
                 │      │              ▼                         │
                 │      │       RuntimeStore  (sui_types         │
                 │      │       storage traits over the overlay) │
                 │      ▼              │                         │
                 │  OverlayStore ◀─────┘   reads                 │
                 │   ├─ local writes (in-memory, latest version) │
                 │   └─ fallthrough ▼                            │
                 └──────────────────┼───────────────────────────┘
                                    │  sui_data_store traits
                                    ▼
                       DataStore  (Mysten sui_data_store, GraphQL)
                                    │
                                    ▼
                       mainnet GraphQL  @ pinned checkpoint
```

### Layers

1. **Backing read store** — `sui_data_store::stores::DataStore`, a GraphQL
   client against `graphql.mainnet.sui.io`. Every object read is pinned to the
   **fork checkpoint** (`VersionQuery::AtCheckpoint(fork_cp)`), so the fork sees
   a single consistent snapshot of mainnet. (`sui-data-store` also offers a
   `ReadThroughStore<InMemoryStore, DataStore>` composition for a persistent
   on-disk/in-memory cache layer; Aquarium uses the bare `DataStore` today, with
   per-execution caching in the `RuntimeStore` — adding the cache layer is a
   drop-in change to `Fork::mainnet`.)

2. **`OverlayStore`** (`src/store.rs`) — the heart of the fork. Implements the
   `sui_data_store` read traits (`ObjectStore`, `EpochStore`, `TransactionStore`).
   - Reads check a local **overlay map** first (objects created/mutated by
     locally-executed transactions, keyed by id → latest `Object`), then a
     **tombstone set** (locally-deleted ids → `None`), then fall through to the
     backing mainnet store.
   - `EpochStore` delegates to the backing store (a fork does not advance
     epochs), so execution uses mainnet's `ProtocolConfig`/rgp for the fork's
     epoch.
   - Writers commit the outputs of an executed transaction into the overlay.

3. **`RuntimeStore`** (`src/engine/runtime_store.rs`, `execute` feature) — a thin
   adapter that exposes a `sui_data_store::ObjectStore` to the Sui `Executor`
   through the *runtime* storage traits the `BackingStore` bound requires:
   `BackingPackageStore`, `ObjectStore` (sui_types), `ChildObjectResolver`, and
   `ParentSync`. This mirrors the private `ReplayStore` in `sui-replay-2`;
   dynamic field / child-object loads read through to the overlay on demand.
   **Adapted from Mysten Labs (see NOTICE).**

4. **`Vm`** (`src/engine/vm.rs`, `execute` feature) — owns the
   `Arc<dyn Executor>` for the fork's protocol version. `execute(tx_data)`:
   1. resolves input objects from `tx_data.input_objects()` against the overlay
      (packages → latest; owned → exact version; shared → latest overlay/fork
      version) and wraps them in `CheckedInputObjects::new_for_replay`;
   2. calls `executor.execute_transaction_to_effects_and_execution_error(...)`
      with the fork epoch, start timestamp, rgp and gas;
   3. returns an `ExecutionOutcome` (status, effects, and the `written`/`deleted`
      object sets to commit). The `Vm` itself does **not** mutate the overlay —
      committing is the caller's job (see `Fork::execute`).

5. **`Sequencer` / `Fork`** (`src/fork.rs`) — top-level handle. Boots a fork at a
   checkpoint, captures epoch metadata, owns the overlay (+ vm under `execute`),
   and exposes `object(id)`, `simulate(tx)` and `execute(tx)` (feature `execute`).
   `execute` runs the VM then atomically commits the outcome's writes/deletions
   to the overlay, serialized per fork (a fork is a serial sequencer).

### What we reuse vs. build

| Piece | Source |
|---|---|
| GraphQL object/tx/epoch fetch + caching + composition | `sui-data-store` (Mysten, as-is) |
| Move VM / adapter | `sui-execution` (Mysten, as-is) |
| Runtime-store adapter shape, input resolution | adapted from `sui-replay-2` (Mysten) |
| Writable overlay, sequencer, fork lifecycle, CLI | **Aquarium (new)** |

## Non-goals

- No real consensus economics. `/epoch/advance` (see `src/cheats.rs`) crosses
  epoch boundaries for `TxContext` and the `SuiSystemState` (`0x5`) epoch, but
  does not settle staking rewards, grow exchange rates, or rotate validators.
- Shared-object congestion control / true consensus version assignment — the
  executor assigns lamport versions from inputs; serial execution makes this safe
  but it is not byte-identical to what consensus would pick under contention.
- Not forgeable: the randomness beacon (`0x8`) and validator DKG can't be
  reproduced (drive them with `/object/set_contents` instead).

## The `serve` façade & cheats

`aquarium serve` (`src/serve.rs`) exposes the fork over `sui.rpc.v2` gRPC
(LedgerService / StateService / MovePackageService / TransactionExecutionService
/ SubscriptionService), with gRPC-Web + CORS. Alongside it, a JSON control API
(`src/control.rs`) provides anvil-style cheats — clock, epoch (+ `0x5` sync),
object override, fund, snapshot/revert, reset, state dump/load, and `/trace` —
built on the same overlay + `engine::Vm`. Reads go through an in-memory
read-through cache (`Fork::for_node`); the overlay can be serialized to disk for
session persistence.

## Verification strategy

Aquarium is validated against **real mainnet** via Sui's public gRPC fullnode
(`fullnode.mainnet.sui.io`, `grpcurl`), independent of the GraphQL path Aquarium
reads through:

- **Read parity** — fetch an object at the fork checkpoint through Aquarium and
  assert its version/digest match `GetObject` from public gRPC.
- **Fork isolation** — execute a transaction locally, assert the overlay now
  holds the mutated object at a new version while public gRPC still shows the
  original. (feature `execute`)
