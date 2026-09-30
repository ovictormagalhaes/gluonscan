# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the workspace releases in lockstep:
one version, one tag, all crates.

## [Unreleased]

## [0.0.1-beta.1]

First public preview of gluonscan — a stateless, multi-chain "read + normalize" engine for DeFi
portfolios. Point it at a wallet; it reads across chains and protocols and returns one normalized
ledger of positions, before any cache or database. APIs are pre-1.0 and may still change.

### Added

- Ports-and-adapters core (`gluonscan-core`): the domain contract, the `Http` / `ChainProvider` /
  `PriceSource` / `Clock` / `ProtocolAdapter` ports, the `Complete<T>` integrity wrapper, and a typed
  fail-closed `Error`.
- Protocol adapters: Aave V3, Uniswap V3, Pendle (EVM), Kamino, Raydium CLMM (Solana).
- Wallet balances and NFTs across EVM, Solana, and Bitcoin.
- Pricing via CoinGecko / CoinMarketCap (native coins, EVM tokens, Solana SPL mints) as a separate
  operation from reading positions.
- Feature-gated facade so consumers compile only the ecosystems they use (`aave`, `uniswap`,
  `pendle`, `kamino`, `raydium`, `prices`, `wallet`; `evm` / `solana` umbrellas; `full` default).

### Guarantees

- Complete-or-error: a reading is returned only when every required source succeeded; partial,
  degraded, or fabricated financial data is never emitted.
- Money is `Decimal` / `U256` end to end — never `f64`. A missing price is an absent value, never a
  fabricated `0` or `1`.

[Unreleased]: https://github.com/ovictormagalhaes/gluonscan/compare/v0.0.1-beta.1...HEAD
[0.0.1-beta.1]: https://github.com/ovictormagalhaes/gluonscan/releases/tag/v0.0.1-beta.1
