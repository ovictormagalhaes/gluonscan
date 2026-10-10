# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html). The workspace releases in
lockstep: one version, one tag, all crates.

## [Unreleased]

## [0.2.0] - 2026-10-09

First stable release of the 0.2 line; the API is the 0.2.0-beta.8 surface plus the changes below.

### Changed

- **Stable `market_id` for Aave V3 and Kamino.** `LendingPosition.market_id` is now set for
  cross-collateralized protocols too: Aave V3 emits the chain's Pool address (lowercase hex; one
  Pool per chain holds the whole account) and Kamino the obligation address (an owner can hold
  several obligations). Consumers no longer need to derive a market identity from the asset mix,
  which moved whenever collateral or liquidation thresholds changed. A Kamino obligation read
  without its address now fails closed.

### Fixed

- **Hyperliquid funding sign.** `PerpPosition.funding` was passed through from
  `cumFunding.sinceOpen` verbatim, but Hyperliquid reports that figure from the exchange's side
  (positive = the position paid funding). The adapter now negates it to match the model's
  convention (negative = paid). Verified live: an ONDO long's `sinceOpen` of `+33.703202` equals
  the negated sum of the account's `userFunding` usdc deltas (`-33.703202`).

## [0.2.0-beta.8] - 2026-10-07

### Added

- **Margin mode and position TP/SL on `PerpPosition`.** New `PerpPosition.margin_mode:
  Option<MarginMode>` (`Isolated` / `Cross`) plus `take_profit_price` and `stop_loss_price`
  (`Option<Decimal>`), with `with_margin_mode` / `with_take_profit_price` / `with_stop_loss_price`
  builders. The Hyperliquid adapter reads the margin mode from the position's `leverage.type` (free,
  same `clearinghouseState` payload), and at `Detail::Full` attaches each position's resting
  take-profit and stop-loss trigger prices from `frontendOpenOrders` (orders Hyperliquid flags as the
  position's own TP/SL). The TP/SL enrichment is **best-effort and advisory**: a transport failure or
  a malformed order leaves the levels `None` and never fails the net-worth-bearing account read. An
  unrecognized margin mode maps to `None` rather than failing closed. `None` for sources that don't
  expose these.

## [0.2.0-beta.7] - 2026-10-05

### Added

- **Token icons on `Token`.** New `Token.logo: Option<String>` with a `with_logo` builder, carrying
  an icon URL when the source serves one for that exact token. The Pendle adapter now reads each PT/YT
  token's `simpleIcon` (falling back to `proIcon`) from the market catalog, so synthetic Pendle tokens
  surface their own icon instead of relying on a downstream address lookup that does not catalog them.
  `None` for sources that don't expose an icon.

## [0.2.0-beta.5] - 2026-10-05

### Changed

- **Ethena staking APY now sourced from DeFiLlama.** Ethena's own app API (`app.ethena.fi`) is not
  reachable from datacenter IPs, so the sUSDe yield silently returned `None` in production. Switched
  to DeFiLlama's CDN-backed, server-reachable yields chart for the stable sUSDe pool; the latest
  point is the current yield. Still best-effort (`None` on any failure).

## [0.2.0-beta.4] - 2026-10-05

### Added

- **Staking APY on `StakePosition`.** New `StakePosition.apy: Option<Decimal>` (fraction, `0.03` = 3%)
  with a `with_apy` builder — the protocol-wide staking yield. The Lido, ether.fi and Ethena adapters
  now fetch it from each protocol's official endpoint and normalize to a fraction. It is **best-effort
  and informational**: a failed fetch leaves `apy = None` and never fails the balance read closed
  (the yield never affects a position's value). `None` for sources that don't expose a rate.

## [0.2.0-beta.3] - 2026-10-05

### Fixed

- **Morpho: never surface a health factor without debt.** Morpho's API can return a stale/inconsistent
  `healthFactor` for a market whose borrow is currently zero (eventual consistency between the HF
  field and the position state). A health factor only exists against debt, so a collateral-only
  position now carries `None` — the exact dual of the existing debt-without-HF guard. Prevents an
  orphaned health factor rendering on a collateral-only card downstream.

## [0.2.0-beta.2] - 2026-10-05

### Added

- **`LendingPosition::market_id`** — the source identity of the isolated market a lending position
  belongs to, with a `with_market_id` builder. The Morpho adapter populates it with the market's
  on-chain `marketId`. It is the only stable identity that distinguishes two isolated Morpho Blue
  markets that share the same collateral, loan asset and LLTV and differ only by oracle/IRM — so a
  consumer that dedups or groups positions can keep them from colliding (a lost position otherwise).
  `None` for cross-collateralized protocols (Aave, Kamino), where one position is the whole account.

## [0.2.0-beta.1] - 2026-10-04

The first release on the `0.2.0-beta` line. It adds a new position kind and a rewards dimension to
the core contract, then **five new protocol adapters** that nearly double the engine's coverage —
from Aave V3, Uniswap V3, Pendle, Kamino and Raydium to also include **Lido, ether.fi, Ethena,
Morpho and Hyperliquid**, spanning liquid staking/restaking, isolated-market lending and perpetuals
across Ethereum, Base and Hyperliquid.

Every new adapter reads on-chain or from a first-party API, normalizes to the domain contract, and
**fails closed** — a partial, degraded or fabricated value is always an error, never a silent `0`.
No money ever routes through `f64`: amounts are exact `U256`/`Decimal`. Valuation stays a separate
operation, so adapters report token amounts (and, for USD-margined perps, the venue's own
authoritative USD figures) and leave pricing to the pricing layer.

### Added

#### Core

- `Position::Perp` + `PerpPosition` / `PerpSide`: a perpetual/derivative position kind (side, size,
  entry/mark/liquidation price, leverage, unrealized PnL, funding). Unset fields stay `None`.
- Claimable `rewards: Vec<Amount>` on `LendingPosition`, `LiquidityPosition` and `StakePosition`
  (emission/incentive tokens, distinct from LP trading fees), with `with_rewards` builders, surfaced
  by the new `Capability::Rewards`.

#### Adapters

- `Lido` (Ethereum): reads stETH + wstETH balances via on-chain `balanceOf` and returns a
  `StakePosition`, with both tokens reported as receipt tokens so a consumer's idle-wallet list does
  not double-count the stake. Rewards auto-compound into the balance, so `rewards` stays empty;
  valuation is left to the pricing layer. Feature `lido`; adds `Protocol::Lido`.
- `EtherFi` (Ethereum): reads weETH + eETH liquid-restaking balances via on-chain `balanceOf` and
  returns a `StakePosition`, with both tokens as receipt tokens. Restaking yield accrues inside the
  balance/rate and ETHFI/EIGEN rewards are off-chain Merkle claims, so `rewards` stays empty.
  Feature `etherfi`; adds `Protocol::EtherFi`.
- `Ethena` (Ethereum): reads the sUSDe (staked-USDe ERC-4626 share) balance via on-chain `balanceOf`
  and returns a `StakePosition`, with sUSDe as a receipt token. Yield accrues in the share price (not
  a separate claimable), so `rewards` stays empty. Feature `ethena`; adds `Protocol::Ethena`.
- `Morpho` (Ethereum, Base): reads Morpho Blue positions from the official GraphQL API and normalizes
  each isolated market into a `LendingPosition` (collateral + debt + per-market health factor; single
  LLTV as both max LTV and liquidation threshold; debt at 100%). Historical/dust markets are dropped;
  a debt position without a health factor, or collateral without its asset metadata, fails closed.
  Base-unit amounts stay exact: Morpho serializes a `BigInt` as a string above 2^53 (parsed verbatim)
  and as a bare number below (fits `u64`). Feature `morpho`; adds `Protocol::Morpho`.
- `Hyperliquid`: reads a wallet's open perpetuals from the public `clearinghouseState` info API and
  normalizes each into a `PerpPosition` (side from the sign of size; mark derived from the returned
  notional; entry/liquidation price, leverage, USD PnL, funding, and USDC collateral). A null
  liquidation price or absent funding stays `None`; a malformed payload or any present-but-unparseable
  field fails closed. First user of `Position::Perp`. Feature `hyperliquid`; adds
  `Protocol::Hyperliquid`.

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

[Unreleased]: https://github.com/ovictormagalhaes/gluonscan/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/ovictormagalhaes/gluonscan/compare/v0.2.0-beta.8...v0.2.0
[0.2.0-beta.8]: https://github.com/ovictormagalhaes/gluonscan/compare/v0.2.0-beta.7...v0.2.0-beta.8
[0.2.0-beta.5]: https://github.com/ovictormagalhaes/gluonscan/compare/v0.2.0-beta.4...v0.2.0-beta.5
[0.2.0-beta.4]: https://github.com/ovictormagalhaes/gluonscan/compare/v0.2.0-beta.3...v0.2.0-beta.4
[0.2.0-beta.3]: https://github.com/ovictormagalhaes/gluonscan/compare/v0.2.0-beta.2...v0.2.0-beta.3
[0.2.0-beta.2]: https://github.com/ovictormagalhaes/gluonscan/compare/v0.2.0-beta.1...v0.2.0-beta.2
[0.2.0-beta.1]: https://github.com/ovictormagalhaes/gluonscan/compare/v0.1.0...v0.2.0-beta.1
[0.1.0]: https://github.com/ovictormagalhaes/gluonscan/compare/v0.0.1-beta.15...v0.1.0
[0.0.1-beta.4]: https://github.com/ovictormagalhaes/gluonscan/compare/v0.0.1-beta.3...v0.0.1-beta.4
[0.0.1-beta.3]: https://github.com/ovictormagalhaes/gluonscan/compare/v0.0.1-beta.2...v0.0.1-beta.3
[0.0.1-beta.2]: https://github.com/ovictormagalhaes/gluonscan/compare/v0.0.1-beta.1...v0.0.1-beta.2
[0.0.1-beta.1]: https://github.com/ovictormagalhaes/gluonscan/releases/tag/v0.0.1-beta.1
