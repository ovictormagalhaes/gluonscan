# Introduction

**gluonscan** is a multi-chain *read + normalize* engine for DeFi portfolios. You point it at a
wallet; it reads across chains and protocols and returns one normalized ledger of positions —
everything *before* any cache or database.

It exists to do one hard thing well: turn the messy, per-protocol, per-chain reality of on-chain
data into a single, trustworthy set of domain types — and never lie about the numbers.

## What makes it different

- **Stateless.** It reads and normalizes; persistence and caching belong to you. The same engine
  serves a backend app, a hosted data API, or a script.
- **Complete or error.** It never returns partial, degraded, or fabricated financial data. A
  missing price is an error, not a silent `0`.
- **Adapters are ports.** A protocol can have several backends (an HTTP API, a subgraph, on-chain
  RPC); you pick, configure, and route capabilities between them.
- **You configure and call; the path is hidden.** Conversions, tick math, and health factors live
  inside the adapters. You get normalized results.

## The shape of a read

```rust,ignore
use gluonscan::{Gluonscan, AaveApi, Protocol, Chain, Detail, Wallet, Address};

let engine = Gluonscan::builder().backend(AaveApi::new()).build();
let ledger = engine
    .read(Protocol::AaveV3, Wallet::Evm(Address::ZERO), Chain::Ethereum, Detail::Full)
    .await?;
```

## Who this is for — choose your path

Different callers use only a slice of the engine. Find your row, jump to the recipe.

| I want to… | Register | Recipe |
|---|---|---|
| Read **one protocol** (e.g. just Aave) | that one backend | [By protocol](recipes/by-protocol.md) |
| Read **everything** a wallet holds | all backends you need | [By protocol](recipes/by-protocol.md) |
| Read strictly **on-chain** (my own node, no third-party APIs) | ⛓️ backends + an `rpc` | [By source](recipes/by-source.md) |
| Read via **hosted APIs** (no node to run) | 🌐 backends | [By source](recipes/by-source.md) |
| Cover **only EVM** chains | EVM backends, `Wallet::Evm` | [By ecosystem](recipes/by-ecosystem.md) |
| Cover **only Solana** | Solana backends, `Wallet::Solana` | [By ecosystem](recipes/by-ecosystem.md) |
| Cover **only Bitcoin** | `BitcoinWallet`, `Wallet::Bitcoin` | [By ecosystem](recipes/by-ecosystem.md) |
| List **idle tokens & NFTs** | wallet / NFT readers | [Wallet & NFTs](recipes/wallet-and-nfts.md) |
| Put a **USD price** on assets | a price source | [Pricing](recipes/pricing.md) |

Not sure what a backend needs? The [coverage matrix](coverage.md) shows each one's source and the
transport you must inject.

## The rest of the book

The [data-integrity contract](concepts/integrity.md), how [detail levels](concepts/detail-levels.md)
control cost, how [providers and wallets](concepts/providers.md) are injected, and how to
[add a protocol adapter](guides/adding-an-adapter.md).
