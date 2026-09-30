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
use gluonscan::{Gluonscan, AaveApi, Chain, Detail, Protocol, Address, Wallet};

# async fn demo() -> Result<(), gluonscan::Error> {
let engine = Gluonscan::builder().backend(AaveApi::new()).build();
let reading = engine
    .read(Protocol::AaveV3, Wallet::Evm(Address::ZERO), Chain::Ethereum, Detail::Full)
    .await?;
println!("{:?}", reading.get());
# Ok(()) }
```

## Pull only what you use

Every protocol adapter sits behind a feature, so you never compile an ecosystem's dependency stack
(EVM's `alloy`, Solana's crypto) that you don't use. The default enables everything for a zero-config
first run; opt out and cherry-pick when you care about build size.

```toml
# Everything (default):
gluonscan = "0.0.1-beta.4"

# Just Aave — pulls no Solana stack:
gluonscan = { version = "0.0.1-beta.4", default-features = false, features = ["aave"] }

# Solana only:
gluonscan = { version = "0.0.1-beta.4", default-features = false, features = ["kamino", "raydium"] }
```

Features: `aave`, `uniswap`, `pendle`, `kamino`, `raydium`, `prices`, `wallet`; ecosystem umbrellas
`evm` (aave + uniswap + pendle) and `solana` (kamino + raydium); and `full` (the default). Features
are additive, so widening them later is never a breaking change.

## License

Licensed under either of [Apache-2.0](../../LICENSE-APACHE) or [MIT](../../LICENSE-MIT) at your
option.
