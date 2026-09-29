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

The rest of this book covers the [data-integrity contract](concepts/integrity.md), how
[detail levels](concepts/detail-levels.md) control cost, how [providers and wallets](concepts/providers.md)
are injected, and how to [add a protocol adapter](guides/adding-an-adapter.md).
