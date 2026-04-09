# Architecture

## Overview

This repository is a Cargo workspace that provides Zcash cryptographic operations
across multiple targets: Node.js/Electron (napi-rs) and a standalone CLI.

## Crate dependency graph

```
zcash-crypto   (pure logic, no FFI, no network I/O)
    │
    └── zcash-sync          (async sync engine — tonic + tokio)
            │
            ├── zcash-ffi-node   (Node.js/Electron — napi-rs cdylib)
            └── zcash-cli        (CLI binary — clap)
```

## Crates

### `zcash-crypto` — the core

Pure Rust cryptographic operations. No FFI, no network I/O. Compiles for all
targets including iOS and Android.

Modules:
- `keys`: BIP-39 → ZIP-32 → UFVK + xpub key derivation
- `decrypt`: Compact block trial decryption and full transaction decryption
- `network`: Zcash network name parsing
- `error`: Unified `Error` enum (thiserror)

This is the only crate subject to the ≥90% test coverage requirement.

### `zcash-sync` — network layer

Async sync client for lightwalletd / Zaino. Converts proto `CompactTx` types
to `zcash-crypto`'s plain data types before calling trial/full decryption.

Depended on by `zcash-ffi-node` and `zcash-cli` for all gRPC operations.

### `zcash-ffi-node` — Node.js/Electron binding

Thin napi-rs `cdylib` wrapper. Exports to JavaScript:
- `startSync(params)` — starts an async block range scan; returns a `TransactionStream`
- `getChainTip(grpcUrl)` — chain tip query
- `TransactionStream` class — async iterator (`next()`) + statistics (`stats()`)

### `zcash-cli` — CLI binary

`clap`-based binary with three subcommands:
- `derive` — key derivation
- `tip` — query chain tip height
- `sync` — block range scan with JSON or human output

## Design decisions

**Why split `zcash-crypto` and `zcash-sync`?**
Keeping tonic/tokio out of `zcash-crypto` ensures the core logic crate compiles
cleanly for every target without heavy networking dependencies. `zcash-sync`
adds gRPC transport on top of it. Both consumer crates (`zcash-ffi-node`, `zcash-cli`)
use `zcash-sync` for network I/O and `zcash-crypto` for pure logic.

**Why define custom compact tx types instead of using proto types?**
`zcash_client_backend`'s proto types require the `lightwalletd-tonic` feature
which pulls in tonic (gRPC framework) and tokio. Defining our own
`CompactTransaction` / `CompactSaplingOutput` / `CompactOrchardAction` types
in `zcash-crypto` keeps it free of these heavy dependencies.

## Adding a new feature

Example: adding an "craft Orchard transaction" function.

1. Implement the logic in `zcash-crypto/src/` (new module, e.g. `craft.rs`).
2. Export it from `zcash-crypto/src/lib.rs`.
3. Add a NAPI wrapper in `zcash-ffi-node/src/lib.rs`.
4. Add a CLI subcommand in `zcash-cli/src/main.rs`.
6. Write tests in `zcash-crypto` (inline or in `tests/`).
7. Update `docs/` as needed.
