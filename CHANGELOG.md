# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html). The workspace releases in
lockstep: one version, one tag, all crates.

## [Unreleased]

### Added

- `Lido` adapter (Ethereum): reads stETH + wstETH balances via on-chain `balanceOf` and returns a
  `StakePosition`, with both tokens reported as receipt tokens so a consumer's idle-wallet list does
  not double-count the stake. Rewards auto-compound into the balance, so `rewards` stays empty;
  valuation is left to the pricing layer. Feature `lido`; adds `Protocol::Lido`.
- `Position::Perp` + `PerpPosition` / `PerpSide`: a perpetual/derivative position kind (side, size,
  entry/mark/liquidation price, leverage, unrealized PnL, funding). Unset fields stay `None`.
- Claimable `rewards: Vec<Amount>` on `LendingPosition`, `LiquidityPosition` and `StakePosition`
  (emission/incentive tokens, distinct from LP trading fees), with `with_rewards` builders, surfaced
  by the new `Capability::Rewards`.

## [0.1.0] - 2026-10-03

First stable (non-beta) release. Promotes the beta line to a stable `0.x` version now that the
engine is in production use via Uniqueledger. Next development continues on the `0.2.0-beta` line.

### Added

- Adapter-completeness regression tests (Uniswap V3 + Aave V3): a produced position must carry
  both token addresses, a two-element `assets` vector, and a status consistent with on-chain
  liquidity; lending legs must carry addresses + risk params + a health factor when in debt.

### Changed

- Minimum supported Rust version raised to 1.90.
- Dependencies refreshed via Dependabot, including `alloy-primitives` 0.8 → 1.
- Workspace clippy lints now require a `reason=` on every `#[allow(...)]`
  (`allow_attributes_without_reason`); the pre-existing suppressions (including `large_enum_variant`
  on `Position`) are now documented with a reason.

## [0.0.1-beta.15] - 2026-10-02

### Added

- Cost-basis chain reads (`transfers`): ERC-20 transfer history, receipt-log transfers, and
  latest-block lookups, so a consumer can derive cost basis from on-chain movements.

## [0.0.1-beta.14] - 2026-10-02

### Added

- `LiquidityPosition` now exposes `sqrt_price`, `tick_spacing`, and `created_at` (Uniswap V3 +
  Raydium), so a consumer can render the price band and position age without re-deriving them.

## [0.0.1-beta.13] - 2026-10-02

### Added

- `LiquidityPosition` now exposes the position `id` and `pool` address (Uniswap V3 + Raydium), so
  two positions in the same pool can be told apart and addressed individually.

## [0.0.1-beta.12] - 2026-10-01

### Added

- Position-scoped `read_history` on the `ProtocolAdapter` contract, with implementations for
  Uniswap V3 and Raydium (snapshot-/on-chain-delta reconstruction into paired single-token events)
  and Kamino (obligation snapshot deltas).
- Lending health-factor + liquidation-price math kernel (`gluonscan-math`).
- Kamino resolves the market from the owner's obligations when the selector omits it.

### Changed

- **Breaking:** `read_history` is now position-scoped (foundation for LP / Kamino history).

## [0.0.1-beta.11] - 2026-10-01

### Added

- Batched pricing: `PriceSource::prices_usd(chain, &[Asset])` (default loops `price_usd`) with a
  `CoinGecko` override that collapses all contract/mint addresses into one `contract_addresses` call
  plus one native call, returning a result per asset in input order. Facade `Gluonscan::prices` and
  the price source both expose it, so a consumer re-pricing many wallet tokens makes ~1 call per
  chain instead of one per token.

## [0.0.1-beta.10] - 2026-10-01

### Changed

- `CoinGecko` price source now always sends a `User-Agent` (CoinGecko's edge 403s agent-less
  requests and the `Http` port does not mandate one) and accepts an optional demo API key via
  `with_api_key` (sent as `x-cg-demo-api-key`) and `with_user_agent`. Added Monad and Hyperliquid to
  the platform / native-coin-id maps.

## [0.0.1-beta.9] - 2026-10-01

### Added

- `AlchemySolanaWallet` — SPL balances (including native SOL, which stays in the token list) via
  Alchemy's Data API, same hygiene and USD attachment as `AlchemyWallet`, plus a per-mint secondary
  price lookup (`/prices/v1/{KEY}/tokens/by-address`) for mints the token endpoint did not price.
  Any price-lookup error fails closed. Native SOL decimals default to 9; a missing SPL decimals from
  a succeeded response fails closed (`Integrity`).

## [0.0.1-beta.8] - 2026-10-01

### Added

- `AlchemyWallet` — EVM idle ERC-20 balances via Alchemy's Data API
  (`/data/v1/{KEY}/assets/tokens/by-address`, key in path, `pageKey` pagination), `Protocol::Wallet`
  / `Source::Api`. Unlike the raw `EvmWallet` (Moralis-shaped), it is batteries-included: applies the
  full wallet hygiene and attaches USD from the bundled prices. Native coins are excluded (read by
  `EvmNativeBalance`). A non-hex balance from a succeeded response fails closed (`Integrity`); an
  astronomical (scam) balance beyond `Decimal` range drops that leaf, never the chain.
- `gluonscan_core::hygiene` — `is_spoofed_token` / `sanitize_display` / `has_disguise_control`:
  detect and strip the Unicode bidi/zero-width/control characters a spoofed token uses to disguise
  its name or symbol (e.g. an RLO rendering "CDSU" as "USDC").

## [0.0.1-beta.7] - 2026-10-01

### Added

- `Protocol::Native` — the chain's native coin balance (ETH, BNB, SOL, BTC, ...) is now its own
  protocol, distinct from `Protocol::Wallet` (idle token balances). This lets one engine route a
  native-balance read and a token-balance read independently on the same chain; previously both
  bound to `Protocol::Wallet` and the engine would pick whichever backend was registered first.

### Changed

- `EvmNativeBalance`, `SolanaNativeBalance` and `BitcoinWallet` now report `Protocol::Native`
  instead of `Protocol::Wallet`. `Protocol` is `#[non_exhaustive]`, so this is additive for
  downstream matches.

## [0.0.1-beta.6] - 2026-10-01

### Changed

- Packaging and presentation only; no API or behavior change. Added the black-and-silver brand
  assets (`assets/logo.{svg,png}`, `assets/icon.{svg,png}`) and a logo + badge header in the README
  (the logo now loads from an absolute URL so it renders on crates.io, not just GitHub). Every
  published crate now ships its own README, a `homepage`, and the facade declares `docs.rs` metadata
  so documentation builds with all features.

## [0.0.1-beta.5] - 2026-09-30

### Changed

- `UniswapV3` now holds a per-chain subgraph map instead of a single endpoint, so one long-lived
  instance can back an engine that routes Uniswap reads per chain. `with_subgraph` takes a `Chain`
  plus its URL; `with_subgraphs(HashMap<Chain, String>)` sets them all at once. A supported chain
  with no configured subgraph fails closed (`Permanent`) rather than hitting a placeholder endpoint.
  This mirrors `AaveApi::with_subgraphs` and lets a single engine serve every chain a backend covers.

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
