# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html). The workspace releases in
lockstep: one version, one tag, all crates.

## [Unreleased]

## [0.0.1-beta.4] - 2026-09-30

### Added

- Event history (`Capability::History`): `History` / `HistoryEvent` / `EventKind` and
  `ProtocolAdapter::read_history` (facade `Gluonscan::history`), a normalized past-events stream
  separate from current positions. Defaults to `Unsupported`; the Aave adapter implements it over a
  per-chain subgraph (`AaveApi::with_subgraphs`) — supply/withdraw/borrow/repay events. Valuation
  and derived analytics (PnL, impermanent loss, cost basis) remain the consumer's.

## [0.0.1-beta.3] - 2026-09-30

### Changed

- `Token.address` is now a chain-agnostic `Option<TokenAddress>` (`enum TokenAddress { Evm(Address),
  Solana(String) }`) instead of `Option<Address>`. Solana adapters (Kamino, Raydium, the Solana
  wallet reader) now populate the SPL mint, which they previously dropped — so consumers can price
  and label Solana tokens by mint. `Token::evm` / `Token::solana` constructors added; `TokenAddress`
  implements `Display` (`0x…` for EVM, base58 for Solana). Breaking for consumers reading
  `token.address`.

## [0.0.1-beta.2] - 2026-09-30

### Fixed

- Pendle (Ethereum): drop the spurious sPENDLE balance read. Its address was not an ERC-20
  (`balanceOf` reverted), which failed closed and blocked every Ethereum Pendle read. The vePENDLE
  lock (locked PENDLE + governance power + unlock time) remains.

## [0.0.1-beta.1] - 2026-09-30

Initial preview release. APIs are pre-1.0 and may change in any release.

### Added

- `gluonscan-core`: domain contract, adapter ports (`Http`, `ChainProvider`, `PriceSource`,
  `Clock`, `ProtocolAdapter`), the `Complete<T>` integrity wrapper, and a typed fail-closed `Error`.
- Protocol adapters: Aave V3, Uniswap V3, and Pendle (EVM); Kamino and Raydium CLMM (Solana).
- Wallet balances and NFTs across EVM, Solana, and Bitcoin.
- Pricing via CoinGecko and CoinMarketCap for native coins, EVM tokens, and Solana SPL mints,
  exposed as an operation separate from reading positions.
- Feature flags on the `gluonscan` facade so consumers compile only the ecosystems they use:
  `aave`, `uniswap`, `pendle`, `kamino`, `raydium`, `prices`, `wallet`; the `evm` and `solana`
  umbrellas; and `full` (default).

[Unreleased]: https://github.com/ovictormagalhaes/gluonscan/compare/v0.0.1-beta.4...HEAD
[0.0.1-beta.4]: https://github.com/ovictormagalhaes/gluonscan/compare/v0.0.1-beta.3...v0.0.1-beta.4
[0.0.1-beta.3]: https://github.com/ovictormagalhaes/gluonscan/compare/v0.0.1-beta.2...v0.0.1-beta.3
[0.0.1-beta.2]: https://github.com/ovictormagalhaes/gluonscan/compare/v0.0.1-beta.1...v0.0.1-beta.2
[0.0.1-beta.1]: https://github.com/ovictormagalhaes/gluonscan/releases/tag/v0.0.1-beta.1
