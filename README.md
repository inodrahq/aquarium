<p align="center">
  <img src="./assets/banner.svg" alt="Aquarium — fork Sui mainnet locally" width="100%">
</p>

<p align="center">
  <a href="https://github.com/inodrahq/aquarium/actions/workflows/ci.yml"><img src="https://github.com/inodrahq/aquarium/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="./LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-blue.svg" alt="License: Apache-2.0"></a>
  <img src="https://img.shields.io/badge/rust-stable-orange.svg" alt="Rust stable">
  <img src="https://img.shields.io/badge/status-alpha-green.svg" alt="Status">
</p>

<h1 align="center">🐠 Aquarium</h1>

<p align="center"><b>Fork Sui mainnet locally.</b> A contained, observable slice of the chain you can poke at without touching the real ocean.</p>

---

Aquarium is to Sui what `anvil --fork-url` is to EVM. It takes **live mainnet
(or testnet / devnet) object state** and lets you execute **new transactions**
against it locally — no validators, no consensus, no multi-terabyte snapshot.
State is fetched **lazily on demand** (through Sui's public GraphQL) and cached;
transactions you run mutate a local **overlay** while the real chain stays
untouched.

`aquarium serve` exposes the fork over the standard **`sui.rpc.v2` gRPC**
surface, so grpcurl, the SDKs and block explorers talk to it like a node — with
**anvil-style cheats** alongside it (advance the clock or epoch, fund an
account, override an object, snapshot/revert, dump/reload, trace a transaction).
Any account can be impersonated (signatures are not verified), so you act as
whoever you need to.

```console
$ aquarium demo --sender 0x1a66…41dd --coin 0xff21…d916 --repeat 4
  tx   1: v920844983 -> v920844984  [SUCCESS]
  tx   2: v920844984 -> v920844985  [SUCCESS]
  tx   3: v920844985 -> v920844986  [SUCCESS]
  tx   4: v920844986 -> v920844987  [SUCCESS]

overlay coin (locally produced after 4 tx(s)):
  version  920844987  (mainnet fork point was 920844983)
# → version 920844987 is NotFound on mainnet: the fork advanced, the chain didn't.
```

## Contents

- [Why a fork?](#why-a-fork)
- [Install](#install)
- [Use](#use)
- [Serve a local node](#serve-a-local-node)
- [Cheat controls](#cheat-controls)
- [Verifying it's real](#verifying-its-real)
- [How it works](#how-it-works)
- [Aquarium vs. anvil](#aquarium-vs-anvil)
- [Status & limitations](#status--limitations)
- [Contributing](#contributing) · [Security](#security) · [License & attribution](#license--attribution)

## Why a fork?

Two halves of "fork the chain and keep going":

- **State is cheap to fetch.** Sui transactions declare their input objects, and
  the Move VM is deterministic given `(inputs, protocol config)`. So Aquarium
  fetches exactly the objects a transaction touches and executes locally.
- **Consensus is skipped.** Continuing the real chain would mean replacing
  mainnet's validator committee with a local one. Aquarium doesn't try — like
  Anvil it is its own trivial sequencer. You lose consensus timing and validator
  economics; you keep faithful Move execution against real state.

See [`DESIGN.md`](./DESIGN.md) for the full architecture.

## Install

**One-liner (wizard):**

```bash
curl -fsSL https://raw.githubusercontent.com/inodrahq/aquarium/main/install.sh | sh
```

A small wizard asks which Sui **channel** you want and whether to install a
prebuilt binary (macOS / Linux, x86_64 / arm64, checksum-verified) or build
from source:

| Channel | What you get |
|---|---|
| `testnet` *(default)* | Same protocol support as mainnet ~a week early — never trails a mainnet protocol activation. |
| `mainnet` | Exact parity with the release mainnet validators run. |
| `devnet` | Bleeding edge, cut from Mysten's tip (source builds only). |

All channels fork **mainnet** state; the channel only selects which Sui release
the Move VM is compiled from (`channels.toml` records the current tags).
Non-interactive installs: set `AQUARIUM_CHANNEL` / `AQUARIUM_METHOD`
(`prebuilt`|`source`), plus `AQUARIUM_VERSION` / `AQUARIUM_INSTALL_DIR`.

**With Cargo (builds from source, testnet channel):**

```bash
cargo install --git https://github.com/inodrahq/aquarium aquarium --locked
```

First build compiles the Sui execution tree (a few minutes). Add
`--no-default-features` for a lightweight, read-only binary (no Move VM).

**Docker:**

```bash
docker run --rm ghcr.io/inodrahq/aquarium info
```

**From source:**

```bash
git clone https://github.com/inodrahq/aquarium && cd aquarium
cargo build --release                  # full binary (incl. the Move VM)
cargo build --no-default-features      # read-only fork (lightweight, faster)
```

## Use

```bash
# Fork metadata: chain id, latest checkpoint, fork point + epoch
aquarium info

# Read any object as the fork sees it (overlay first, else mainnet@fork)
aquarium object --id 0x6                          # the Clock
aquarium object --id 0x5 --checkpoint 288207778   # at a pinned checkpoint

# Execute transaction(s) against the fork (full binary only).
# Runs gas-only tx(s) and shows the overlay mutate while mainnet doesn't.
# --repeat N chains N transactions, each consuming the previous one's output.
aquarium demo \
  --sender 0x1a66…41dd \
  --coin   0xff21…d916 \
  --checkpoint 288207778 \
  --repeat 4
```

| Command | What it does |
|---|---|
| `info` | Chain id, latest checkpoint, chosen fork point + its epoch. |
| `object --id <id>` | Read an object as the fork sees it (overlay first, then mainnet). |
| `demo` | Execute gas-only transaction(s) and prove fork isolation. |
| `serve` | Serve the fork over `sui.rpc.v2` gRPC + the cheat control API. |

Every command takes `--network mainnet|testnet|devnet|<graphql-url>` (default
`mainnet`) and an optional `--checkpoint N` to pin the fork point.

The library API (`Fork`, `Fork::execute` / `simulate`, `OverlayStore`, `engine::Vm`)
lets you build your own transactions and drive the fork programmatically.

## Serve a local node

```bash
aquarium serve                       # fork mainnet @ latest, gRPC :9123, cheats :9124
aquarium serve --network testnet     # fork testnet instead
aquarium serve --checkpoint 296136961 --port 9123   # pin the fork point
```

The fork is served over the standard **`sui.rpc.v2`** gRPC surface (with
reflection, and gRPC-Web + CORS so browsers/explorers can reach it):

| Service | Methods |
|---|---|
| `LedgerService` | `GetServiceInfo`, `GetObject`, `BatchGetObjects`, `GetTransaction`, `BatchGetTransactions`, `GetCheckpoint`, `GetEpoch` |
| `StateService` | `GetBalance`, `ListBalances`, `ListOwnedObjects`, `GetCoinInfo`, `ListDynamicFields` |
| `MovePackageService` | `GetPackage`, `GetDatatype`, `GetFunction`, `ListPackageVersions` (parsed from on-fork bytecode) |
| `TransactionExecutionService` | `ExecuteTransaction`, `SimulateTransaction` — **signatures not verified** |
| `SubscriptionService` | `SubscribeCheckpoints` over the fork's synthetic feed |

```bash
grpcurl -plaintext 127.0.0.1:9123 sui.rpc.v2.LedgerService.GetServiceInfo
grpcurl -plaintext -d '{"object_id":"0x6"}' 127.0.0.1:9123 \
  sui.rpc.v2.LedgerService.GetObject
```

Point the `@mysten/sui` SDK (or a block explorer) at `127.0.0.1:9123` and it
behaves like a node — except you can execute as any account and drive state with
the cheats below.

## Cheat controls

A small JSON/HTTP API on `--control-port` (default gRPC port **+ 1**), kept off
the `sui.rpc.v2` surface so that stays a faithful node twin. The anvil analogy:

| Endpoint | Body | Effect (anvil equivalent) |
|---|---|---|
| `GET /status` | — | clock, epoch, `0x5` epoch, fork point, tx count |
| `POST /fund` | `{address, amount, coin_type?}` | mint a coin into an account (`anvil_setBalance`) |
| `POST /clock/set` | `{timestamp_ms}` | pin the `Clock` (`evm_setTime`) |
| `POST /clock/advance` | `{delta_ms}` | bump the clock (`evm_increaseTime`) |
| `POST /clock/auto` · `/clock/freeze` | — | resume real-time drift / freeze |
| `POST /epoch/advance` | `{count?, timestamp_ms?}` | cross epoch boundaries (advances `TxContext` **and** `0x5`) |
| `POST /object/set_contents` | `{object_id, contents_base64, bump_version?}` | overwrite any object (`anvil_setStorageAt`) |
| `POST /snapshot` → `{id}` · `POST /revert` | `{id}` | capture / roll back state (`evm_snapshot` / `evm_revert`) |
| `POST /reset` | — | clear the overlay + epoch/clock to the fork point (`anvil_reset`) |
| `POST /state/dump` · `/state/load` | `{path}` | persist / reload the fork session to disk |
| `POST /trace` | `{transaction, full?}` | dry-run + execution trace (commands, gas, object changes, full Move trace) |

```bash
# Fund a fresh account with 1000 SUI — no whale needed
curl -XPOST 127.0.0.1:9124/fund -H 'content-type: application/json' \
  -d '{"address":"0xabc…","amount":1000000000000}'

# Cross two epochs (unblocks epoch-gated staking, etc.)
curl -XPOST 127.0.0.1:9124/epoch/advance -H 'content-type: application/json' \
  -d '{"count":2}'

# Snapshot, run some transactions, then roll back
curl -XPOST 127.0.0.1:9124/snapshot          # → {"id":0}
# … execute txs …
curl -XPOST 127.0.0.1:9124/revert -H 'content-type: application/json' -d '{"id":0}'
```

Because a fork is frozen at a checkpoint (no consensus advances the clock,
epoch, or randomness beacon), these let you drive that state yourself. Oracles
work the anvil way too: a forked price feed is stale just like a forked
Chainlink feed — impersonate the updater or `/object/set_contents` the price
object to make it fresh.

## Verifying it's real

Aquarium reads through Sui's GraphQL endpoint; we verify against the
**independent public gRPC fullnode** so the two never share a path.

```bash
# 1. What Aquarium reads at the fork point …
aquarium object --id 0x6 --checkpoint 288207778
#   version 849923153  digest F9xTJRi2GTL2KEt4f22dn2jUec8LvA7yF83iNBbmJth2

# 2. … is the chain's object, byte for byte:
grpcurl -d '{"object_id":"0x0000…0006","version":849923153,
             "read_mask":{"paths":["digest"]}}' \
  fullnode.mainnet.sui.io:443 sui.rpc.v2.LedgerService.GetObject
#   "digest": "F9xTJRi2GTL2KEt4f22dn2jUec8LvA7yF83iNBbmJth2"   ✅ identical
```

After `aquarium demo`, the gas coin's **new** version is `NotFound` on mainnet
gRPC — the fork advanced its state without touching the real chain.

## How it works

```
   new tx ─▶ Fork::execute ─▶ Vm (sui-execution Move VM)
                 │                │
                 │                ▼
                 │         RuntimeStore  (sui_types storage traits)
                 ▼                │
            OverlayStore ◀────────┘  reads
             ├─ local writes (in-memory, latest version)
             └─ fallthrough ▼
                        DataStore (Mysten GraphQL)  @ pinned checkpoint
```

- **`OverlayStore`** — reads resolve overlay-first, then mainnet@checkpoint;
  executed-tx outputs are committed into the overlay atomically.
- **`RuntimeStore`** — adapts the overlay to the Move VM's storage traits;
  child/dynamic-field loads read through on demand.
- **`Vm` / `Fork`** — a serial sequencer: resolve inputs → execute → commit.

Full detail and the reuse-vs-build breakdown are in [`DESIGN.md`](./DESIGN.md).

## Aquarium vs. anvil

| | `anvil --fork-url` (EVM) | Aquarium (Sui) |
|---|---|---|
| State source | lazy `eth_getStorageAt` | lazy GraphQL object reads (cached) |
| Execution | EVM | real Move VM (`sui-execution`) |
| Consensus | none (instant mine) | none (serial sequencer) |
| Networks | any RPC url | mainnet / testnet / devnet / custom |
| Standard RPC | JSON-RPC | full `sui.rpc.v2` gRPC |
| Impersonation | `anvil_impersonateAccount` | any account (sigs not verified) |
| Time / storage cheats | `evm_setTime` / `setStorageAt` | `/clock/*` · `/object/set_contents` |
| Fund an account | `anvil_setBalance` | `/fund` |
| Snapshot / revert | `evm_snapshot` / `evm_revert` | `/snapshot` · `/revert` |
| Reset | `anvil_reset` | `/reset` |
| Persist session | `--dump-state` / `--load-state` | `/state/dump` · `/state/load` |
| Trace a tx | `debug_traceTransaction` | `/trace` |
| Sui-specific | — | `/epoch/advance` (+ `0x5` sync) |
| Mutates real chain | no | no |

## Status & limitations

Reads are verified byte-for-byte against mainnet; transaction execution runs the
real Move VM and has been driven against live DeepBook / Cetus / Navi / Haedal
bytecode, native staking, and package publishing. The `serve` surface and cheats
are exercised end-to-end. Honest scope:

- **No real consensus/economics.** The fork is a serial sequencer. `/epoch/advance`
  crosses epoch boundaries for `TxContext` and `0x5`, but does **not** settle
  staking rewards, grow validator exchange rates, or rotate the validator set.
- **Frozen beacon.** Randomness (`0x8`) can't be forged (no validator DKG), just
  as anvil can't forge a Chainlink round — use `/object/set_contents` to drive it.
- **Protocols with off-chain cranks** (e.g. Haedal delayed unstake) can't be
  advanced past a settlement the fork never runs.
- Input objects are taken at the versions a transaction pins (validation
  bypassed), like Sui transaction replay — build against current versions.
- The read cache is in-memory (fast within a session); use `/state/dump` to
  persist a session across restarts. A disk cache of fetched state is a possible
  future addition.
- Linux release binaries are glibc; Alpine/musl users build via `cargo install`.

## Contributing

PRs welcome — see [`CONTRIBUTING.md`](./CONTRIBUTING.md) for build, test, and
style (`cargo fmt` + `cargo clippy -D warnings` + `cargo test`, both feature
sets). Please read the [Code of Conduct](./CODE_OF_CONDUCT.md).

## Security

Aquarium is a developer tool that **bypasses transaction validation** to replay
against real state — never use it to sanction or authorize anything on the live
chain. Report vulnerabilities per [`SECURITY.md`](./SECURITY.md).

## License & attribution

Licensed under the **Apache License 2.0** ([`LICENSE`](./LICENSE)).

Aquarium is built on, and links against, **Mysten Labs'** Sui crates
(`sui-data-store`, `sui-types`, `sui-execution`), and its execution glue is
**adapted from** Mysten's `sui-replay-2`. Those portions remain copyright Mysten
Labs, Inc. under Apache-2.0; see [`NOTICE`](./NOTICE) for the full attribution.
Sui and Move are projects of Mysten Labs — Aquarium is an independent tool and is
not affiliated with or endorsed by Mysten Labs.
