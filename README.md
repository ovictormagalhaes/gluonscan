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
use gluonscan::{Gluonscan, AaveApi, Chain, Detail, Protocol, Address};

let engine = Gluonscan::builder().backend(AaveApi::new()).build();
let reading = engine
    .read(Protocol::AaveV3, owner, Chain::Ethereum, Detail::Full)
    .await?;
```

## Status

Early. First vertical slice: **Aave V3 on Ethereum** (API backend). Next: all chains Aave supports,
then the other protocols one by one.

## License

Dual-licensed under either [MIT](LICENSE-MIT) or Apache-2.0, at your option.
