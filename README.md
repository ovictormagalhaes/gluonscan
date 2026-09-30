<img src="book/theme/favicon.svg" width="76" align="right" alt="gluonscan mark" />

# gluonscan

Multi-chain **read + normalize** engine for DeFi portfolios. Point it at a wallet; it reads across
chains and protocols and returns one normalized ledger of positions — everything *before* any cache
or database.

- **Stateless.** It reads and normalizes; persistence and caching are yours.
- **Complete or error.** It never returns partial, degraded, or fabricated financial data.
- **Adapters are ports.** A protocol can have several backends (API, on-chain, subgraph); you pick,
  configure, and route capabilities between them.
- **You configure and call; the path is hidden.** Conversions, tick math and health factors live
  inside the adapters — you get normalized results.

```rust
use gluonscan::{Gluonscan, AaveApi, Address, Chain, Detail, Protocol, Wallet};

let engine = Gluonscan::builder().backend(AaveApi::new()).build();
let reading = engine
    .read(Protocol::AaveV3, Wallet::Evm(Address::ZERO), Chain::Ethereum, Detail::Full)
    .await?;
```

## Coverage

Protocols: **Aave V3**, **Uniswap V3**, **Pendle** (EVM), **Kamino**, **Raydium CLMM** (Solana).
Wallet balances and NFTs across **EVM, Solana, and Bitcoin**. Pricing via **CoinGecko** /
**CoinMarketCap** (native coins, EVM tokens, and Solana SPL mints), as a separate operation. See the
[coverage matrix](book/src/coverage.md) for each backend's source and required transport.

## Different callers use different slices

Read one protocol or all of them; strictly on-chain over your own node or via hosted APIs; only EVM,
only Solana, only Bitcoin — the [recipes by use case](book/src/recipes.md) show each path in a few
lines.

## Status

Early but broad. APIs are pre-1.0 and may still change.

## License

Dual-licensed under either [MIT](LICENSE-MIT) or Apache-2.0, at your option.
