# Contributing to Aquarium

Thanks for your interest! Aquarium is a small, focused tool — contributions that
keep it sharp (correctness, faithful fork behavior, good DX) are very welcome.

By participating you agree to abide by our [Code of Conduct](./CODE_OF_CONDUCT.md).

## Prerequisites

- A recent **stable Rust** toolchain (`rustup` recommended; the repo pins
  `stable` via `rust-toolchain.toml`).
- Network access at runtime (Aquarium reads `graphql.mainnet.sui.io`).
- Optional: [`grpcurl`](https://github.com/fullstorydev/grpcurl) to reproduce the
  mainnet-parity verification.

## Build, test, lint

```bash
cargo build                      # full binary (default features = execute)
cargo build --no-default-features  # lightweight read-only build (no Move VM)
cargo test                       # offline overlay unit tests
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo clippy --all-targets --no-default-features -- -D warnings
```

CI runs fmt + clippy (both feature sets) + tests. Please make sure all of the
above pass before opening a PR. The first build of the `execute` feature
compiles the Sui execution tree and is slow; subsequent builds are fast.

## Project layout

| Path | What |
|---|---|
| `src/store.rs` | `OverlayStore` — the writable fork overlay (overlay-first reads, atomic commit). |
| `src/fork.rs` | `Fork` / `MainnetFork` — the top-level handle and serial sequencer. |
| `src/gql.rs` | Narrow GraphQL client for checkpoint metadata. |
| `src/engine/` | Transaction execution (`Vm`, `RuntimeStore`) — gated behind the `execute` feature. |
| `src/main.rs` | The CLI. |
| `DESIGN.md` | Architecture and design rationale. |

## Style & conventions

- Match the surrounding code; keep comments explaining *why*, not *what*.
- Every source file starts with an SPDX header:
  `// SPDX-License-Identifier: Apache-2.0`.
- Keep the read-only build working: anything that pulls in `sui-execution` /
  `prometheus` must live behind `#[cfg(feature = "execute")]`.
- Prefer deep fixes over special cases (a fork is a serial sequencer — model it
  that way rather than patching symptoms).

## Working with Mysten-derived code ⚠️

Aquarium adapts code from Mysten Labs' Apache-2.0 `sui-replay-2`
(currently `src/engine/runtime_store.rs` and `src/engine/vm.rs`). If you modify
or add code that is copied or closely adapted from the Sui repository:

- Keep the `// Portions Copyright (c) Mysten Labs, Inc.` header on those files.
- Note the upstream source in a comment, and update [`NOTICE`](./NOTICE) if you
  introduce a new derived file.
- Original Aquarium code carries `// Copyright (c) Inodra` only.

When in doubt, over-attribute. See [`NOTICE`](./NOTICE) for the current state.

## Commits & pull requests

- Keep PRs focused; one logical change per PR.
- Write clear commit messages (imperative mood: "add …", "fix …").
- We use the **Developer Certificate of Origin** — sign off your commits with
  `git commit -s` (adds a `Signed-off-by:` line) to certify you have the right
  to submit the code under Apache-2.0.
- Describe what you changed, why, and how you verified it (include the commands /
  output for any mainnet checks).

## Good first contributions

- Additional CLI niceties (e.g. a `simulate` subcommand, JSON output).
- A `Fork`-backed JSON-RPC/gRPC façade so wallets can point at localhost
  (sketched in `DESIGN.md`).
- A `ReadThroughStore` disk cache layer in front of the GraphQL `DataStore`.
- musl static-linking for Alpine release binaries.

Thanks for helping make local Sui development better. 🐠
