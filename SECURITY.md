# Security Policy

## Supported versions

Aquarium is pre-1.0; only the latest release on `main` is supported. Security
fixes land on `main` and in the next tagged release.

## What Aquarium is (and what it is not)

Aquarium is a **local developer tool**. It reads public mainnet state and runs
transactions **entirely on your machine**, against an in-memory overlay. It
holds no keys, signs nothing, and never submits anything to the live chain.

By design, Aquarium **bypasses transaction validation** (it executes via the
Move VM's replay path, like `sui replay`). This means it will happily execute
transactions that the real network would reject — wrong owners, stale object
versions, unfunded gas, etc. **That is intended for local experimentation and is
not a vulnerability.** Never treat an Aquarium execution result as evidence that
a transaction is valid, authorized, or safe on mainnet.

## Reporting a vulnerability

Please report security issues **privately** — do not open a public issue.

1. Preferred: open a private report via **GitHub Security Advisories**
   (the repository's *Security* tab → *Report a vulnerability*).
2. Alternatively, email the maintainers at the security contact listed in the
   repository profile.

Please include: affected version/commit, a description, reproduction steps, and
the impact you observed. We aim to acknowledge reports within **5 business days**
and to provide a remediation timeline after triage. We'll credit reporters who
wish to be acknowledged once a fix ships.

## Scope

In scope:

- Memory-safety or panics reachable from normal CLI use or the public library API.
- Incorrect state/version handling that produces results diverging from a
  faithful local fork (e.g. wrong object served, lost overlay write).
- Supply-chain issues in the installer (`install.sh`) or release workflow
  (e.g. a path that installs an unverified binary).

Out of scope:

- The intentional validation bypass described above.
- Behavior of the upstream Sui / Mysten crates Aquarium depends on — report
  those to the [Sui project](https://github.com/MystenLabs/sui).
- Reliance on third-party public endpoints (`graphql.mainnet.sui.io`,
  `fullnode.mainnet.sui.io`) being available or trustworthy.
