# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html). The workspace releases in
lockstep: one version, one tag, all crates.

## [Unreleased]

## [0.3.0-beta.4] - 2026-10-18

### Added

- **`EventKind::SupplyInterest` and `EventKind::BorrowInterest`.** A supplied balance that grew by
  the interest it earned, and a borrowed balance that grew by the interest owed, between two
  readings. Neither is an action of the owner; the amount is the growth of the balance.
- **`gluonscan_kamino::MAX_ACCRUAL_PER_DAY`** (0.2%/day), the largest daily growth of a held
  balance, as a fraction of the previous balance, that Kamino history reads as interest.

### Changed

- **Breaking (behaviour): Kamino history reports interest accrual as interest.** History is the
  per-mint delta between consecutive `metrics/history` points, so a supplied or borrowed balance
  produced a Deposit or Borrow event at every point for its accrued interest (on a live obligation,
  about 300 of each next to a handful of real actions). A rise of a held balance at or under
  `MAX_ACCRUAL_PER_DAY` times the exact time between the two points is now `SupplyInterest`
  (deposits) or `BorrowInterest` (borrows) of the growth, with a synthetic id of its own
  (`kamino:{obligation}:{ts}:supply_interest|borrow_interest:{mint}`). A rise above that bound
  stays a Deposit / Borrow of the whole delta (the period's accrual included), as does a rise from
  a zero balance; every fall stays a Withdraw / Repay. Amounts are unchanged. Consumers matching
  `EventKind` must handle the two new kinds (the enum is `#[non_exhaustive]`, so a wildcard arm
  already compiles, but would now see accrual there instead of under Deposit / Borrow).

## [0.3.0-beta.3] - 2026-10-18

### Added

- **`gluonscan_aave::HISTORY_PAGE_SIZE` and `MAX_HISTORY_PAGES`**, the page size and per-kind page
  limit of the paginated Aave history read.

### Fixed

- **Kamino: history with fractional amounts.** `read_history` parsed each `metrics/history` amount
  as an integer and read anything else as zero, while the API prints amounts with accrued fractions
  of a base unit (`"5200000000.42793513"`). A history of fractional points produced no events; a mix
  of integer and fractional points produced Deposit/Withdraw/Borrow/Repay events of the full balance
  around every fractional point. Amounts are now whole base units, the fraction dropped exactly as
  the live reader drops it.
- **Breaking (behaviour): Kamino amount parsing, live reader and history.** Amounts are read by one
  parser for both. A junk or second fraction (`"5.junk"`, `"5.3.2"`), which used to pass on its
  integer part, is now rejected (`Error::Integrity` at the caller). Exponent form with a negative
  exponent, which the API prints for values under 1e-7 (`"4e-7"`, `"5E-8"`), is now read as its whole
  base units (0 below one base unit) instead of failing: a dust leg in that form no longer fails a
  live read or a whole history. A sign, or a zero or positive exponent, is still rejected.
- **Breaking (behaviour): Kamino history fails closed on an unreadable point.** A missing or
  non-array `history`, a point without an RFC 3339 `timestamp` or a `deposits` / `borrows` array, a
  leg without `mintAddress`, or an amount that is not a non-negative decimal is now
  `Error::Integrity`. A missing `history` array used to read as an empty history, and the others
  as a zero balance.
- **Breaking (behaviour): Kamino history checks the selector and the obligation's owner.** Every
  part of the `"obligation"` / `"obligation|market"` selector, and the wallet, must be base58, or
  the read is refused as `Error::Permanent` before any request. With a market supplied, the
  obligation must be listed among the owner's obligations in that market
  (`users/{wallet}/obligations`, the list the market-resolving form already used): one that is not
  is `Error::Permanent`, and a failed ownership read is an error, never a history. The obligation
  match is now exact (base58 is case-sensitive) in both forms.
- **Breaking (behaviour): Aave history reads every event, at one block.** Each event kind was one
  page of 500 with no pagination, while consumers checkpoint on the newest event across kinds: an
  account with more than 500 supplies and a newer repay lost every supply past the 500th for good.
  Each kind is now paged by `id` (`HISTORY_PAGE_SIZE` per page) until a page comes back short, and
  the kinds are merged oldest-first. The first page asks for `_meta { block { number } }` and every
  later page is pinned to that block (`block: { number }`), so a gateway serving later pages from a
  lagging indexer cannot drop events. These now fail the whole read: any failed page, a page with
  GraphQL errors, a requested kind absent from the response (which used to read as no events), an
  event without `id`, a page repeating an event, a first page without `_meta.block.number`
  (`Error::Integrity`), and a kind still open after `MAX_HISTORY_PAGES` pages (`Error::Integrity`).
  A later page whose indexer has not reached the pinned block yet is a retryable
  `Error::Transient`.

## [0.3.0-beta.2] - 2026-10-17

### Added

- **`StakePosition::underlying`: what a wrapped stake redeems for.** Index-aligned with `staked`:
  wstETH carries the stETH it unwraps to (`getStETHByWstETH`), weETH the eETH (`getEETHByWeETH`),
  sUSDe the USDe (`convertToAssets`). A consumer can tell the staking yield (the underlying growing
  at a constant wrapper balance) from a price move. Rebasing tokens (stETH, eETH) carry `None`, as
  does a wrapper whose rate read failed: like `rates`, the rate never fails the balance read.
- **`gluonscan_evm::convert_amount` and `encode_selector_with_u256`**, the best-effort
  `uint256 -> uint256` view read behind it.
- **`gluonscan_morpho::holds_market`.** Asks the Morpho Blue singleton (`position(id, user)`,
  and `market(id)` when only supply shares remain) whether a wallet still holds anything in a market, so a consumer can confirm a market that
  vanished from the API before accepting it as closed. Any failure is an error, never `false`.
  The facade exposes it over the engine's RPC as `Gluonscan::morpho_holds_market` (behind the
  `morpho` feature). A chain outside Ethereum and Base is refused as `Error::Permanent`. Supply
  shares worth less than one base unit of the loan asset (what withdrawing by assets can leave) are not
  a holding, matching the API, which reports that market as empty.
- **`WalletBalance::gated` and `WalletGate`.** Marks a balance a hygiene gate would hide.

### Changed

- **Breaking (behaviour): Alchemy wallet readers report gated balances instead of dropping them.**
  An unpriced unverified token is reported with no USD amount and `gated: Some(Unpriced)`; a balance
  worth less than the $1 dust floor keeps its value and `gated: Some(BelowDust)`. A held token whose quote
  dipped under the floor or went missing for a day used to vanish and come back, which read as a
  transfer out and in. Spoofed symbols and implausible values or prices are still dropped.
  Consumers that want the old behaviour drop every leaf with `gated.is_some()`.

### Fixed

- **Morpho: a null user or a null amount is the API failing, not an empty wallet.** The live API
  answers a never-seen address with an empty `marketPositions` list, so `userByAddress: null` now
  returns a retryable `Error::Transient` instead of an empty reading, and a null `supplyAssets`,
  `borrowAssets` or `collateral` no longer reads as zero (which could drop a live market as empty).

## [0.3.0-beta.1] - 2026-10-15

### Changed

- **Breaking: rates carry their basis, window, source and timestamp.** `SuppliedAsset::apy`,
  `BorrowedAsset::apy`, `YieldPosition::apy`, `StakePosition::apy` and `LiquidityPosition::apr`
  (`Option<Decimal>`) and their `with_apy` / `with_apr` builders are replaced by `rates: Rates` with
  `with_rates(Rates)`, `with_rate(Rate)` and a `rates()` accessor. A `Rate` is a fraction plus its
  `RateBasis` (`Apy` / `Apr`, never converted into each other), its `RateWindow` (`Instant`,
  `Trailing { hours }`, `Epoch { hours }`, `Implied`), the `Source` that produced it (independent of
  the reading's own source, so an API rate on an on-chain reading is labelled as such) and `as_of`
  (the upstream timestamp when the response has one, else the read time). `Rates` holds at most one
  rate per `(basis, window)`: `insert` replaces and returns a duplicate, ordering is by basis then
  window, and `get(basis, window)` never substitutes another window. There is no single-number
  accessor: a consumer picks the window it means.
- **Morpho collateral is an explicit `0` Instant APY sourced `OnChain`.** Morpho Blue never lends
  collateral (`supplyCollateral` only credits `position.collateral`, which accrues no interest), so
  the zero is a contract rule rather than an API value. A yield-bearing collateral token's own yield
  (the API's `collateralAsset.yield.apr`) is not reported here.

### Fixed

- **Lido and ether.fi rates are APRs.** Both publish simple APRs that were stored as APY. Lido's
  `apr/last` is now an `Apr` over a 24h `Epoch` dated at the oracle report's `timeUnix`; ether.fi's
  latest APR is an `Apr` over an `Epoch` of unstated length, dated at the read (the API returns no
  timestamp).
- **Ethena's yield is dated at its DeFiLlama point**, not at the read, as an `Apy` over a 24h
  `Epoch`. Lido, ether.fi and Ethena parse the published number from serde_json's shortest round-trip
  literal instead of by `f64` arithmetic.

### Added

- **Kamino trailing APYs.** For every reserve a wallet supplies or borrows, the adapter fetches the
  reserve's hourly `metrics/history` for the last 7 days (at most 4 requests at once) and adds 24h
  and 168h `Trailing` supply/borrow APYs, the mean of `supplyInterestAPY` / `borrowInterestAPY` over
  the window ending at the newest point and dated at it. A window is emitted only when it holds at
  least `MIN_TRAILING_COVERAGE_PCT` (90%) of its clock hours and the newest point is not in the
  future and at most `MAX_RATE_POINT_AGE_SECS` (3h) old. The instantaneous APY from `reserves/metrics` stays as
  `Instant`. A failed or malformed history leaves the trailing rates out and never fails the read.
- **Lido 7-day APR**: `apr/sma`, the mean of the last 7 daily oracle reports, as a 168h `Trailing`
  `Apr` dated at the newest report. Omitted when the response lists fewer than 7 reports or the
  newest is more than 48h old.
- **Ethena 7-day APY**: the mean of the newest 7 distinct UTC days of DeFiLlama points (the newest
  point of each day), as a 168h `Trailing` `Apy`, emitted when those days span at most 8 days and
  the newest is at most 48h old.
- **Morpho trailing APYs**: the market state's `dailySupplyApy` / `dailyBorrowApy`,
  `weeklySupplyApy` / `weeklyBorrowApy` and `monthlySupplyApy` / `monthlyBorrowApy` (rewards
  excluded) as 24h, 168h and 720h `Trailing` APYs, dated with the instantaneous ones at the state's
  `timestamp`. A null field leaves that window out.

## [0.2.5] - 2026-10-13

### Fixed

- **Uniswap V3 mirrored zero collected fees are detected.** The subgraph mirror copies token0's
  collected total into token1 even when token0's is zero, so a position that only ever collected
  token1 read as 0/0 and its collects were silently lost. `read_history` now checks a stream whose
  collected totals are zero on both sides after any activity past its first snapshot against the
  `Collect` receipts, under the same acceptance rules as 0.2.3 (the token0 sum must reproduce the
  subgraph's token0 change, here zero); without an RPC or on a mismatch the collects are withheld.
  A Full read of an open position with zero totals rebuilds its lifetime collects from the receipts
  too, unless its only snapshot is the creation one; a closed one with withdrawn principal (which a
  collect includes) is marked unknown. Without an RPC, a history whose totals are zero on both
  sides after any activity past its first snapshot is now returned with its collects withheld
  rather than as complete.

## [0.2.4] - 2026-10-13

### Fixed

- **Kamino amounts and values are live again.** Each leg now comes from the newest point of the
  obligation's hourly `metrics/history`: the underlying token amount with interest accrued and its
  refreshed USD value. Before, the adapter read the obligation account `state`, where deposits are
  collateral shares (understating the underlying by the reserve exchange rate) and debt and market
  values are frozen at the owner's last transaction, so debt never accrued and USD values stayed
  fixed while prices moved.
- **The read fails closed when the point is not live.** A newest point older than
  `MAX_POINT_AGE_SECS` (4h; the API publishes each hourly point about an hour late), a missing point, or a transaction after the point returns a retryable
  `Error::Transient`. A point whose reserves differ from the account's, a leg without a USD value,
  an account without `state.lastUpdate.timestamp` or without its leg arrays returns
  `Error::Integrity`. A leg under one base unit is skipped before its USD is read, and USD values in
  exponent notation are accepted. The health factor
  now comes from the same point as the legs. An emptied obligation is read without a point.

## [0.2.3] - 2026-10-13

### Fixed

- **Uniswap V3 mirrored collected fees are rebuilt from receipts.** When the subgraph mirrors
  `collectedFeesToken0` into `collectedFeesToken1`, the true amounts are read from the
  NonfungiblePositionManager `Collect` logs in the receipts of the position's snapshot txs, which
  needs no `eth_getLogs` range. Like the subgraph's totals, a `Collect` includes withdrawn principal.
  The rebuild is accepted only when its token0 sum reproduces the subgraph's token0 total (which the
  mirror leaves intact). `read_history` then returns the rebuilt collects; a missing or failed
  receipt fails the history (retryable). A stream the receipts cannot fully account for (no RPC,
  malformed or truncated snapshots, a tx without a hash, a token0 mismatch) keeps the collects
  withheld with `collected_fees_unavailable`, as before. An open position read at `Detail::Full`
  rebuilds its lifetime collected totals the same way; any failure leaves only that position
  unavailable. Closed positions keep the flag.

### Added

- **`transfers::receipt_logs`** in `gluonscan-evm`: every log of a receipt. A receipt the node does
  not return is a retryable `Error::Transient`, never an empty list.

## [0.2.2] - 2026-10-13

### Added

- **`LendingPosition.parent_market`**, with a `with_parent_market` builder: the protocol market that
  holds the account when `market_id` identifies an account rather than a market. Kamino sets it to
  the lending market of each obligation, so a consumer can call `read_history` with
  `"obligation|market"` and skip the scan across every known market. `None` for Aave and Morpho,
  whose `market_id` already is the market.

## [0.2.1] - 2026-10-12

### Added

- **`is_dust(raw, decimals)` in `gluonscan-core`.** A raw balance below `10^(decimals - 9)` base
  units (1 gwei for an 18-decimal token; exactly zero for tokens with fewer than 9 decimals) is
  share-rounding residue, not a holding. Raw units only, never USD, so an unpriced balance is never
  mistaken for dust; a threshold that would overflow `U256` keeps the balance. Not for debt.
- **`LiquidityPosition.collected_fees_unavailable` and `History.collected_fees_unavailable`**, with
  `with_collected_fees_unavailable` builders. `true` means the collected-fee history could not be
  trusted and was withheld: treat `collected_fees` (or the missing `CollectFees` events) as
  **unknown, not zero**.

### Changed

- **Uniswap V3 corrupted collected fees degrade per position instead of failing the read.** The
  subgraph mirrors `collectedFeesToken0` into `collectedFeesToken1` for many Ethereum positions. The
  adapter used to return `Error::Integrity` for the whole chain read; it now keeps the position
  (liquidity, principal, deposited/withdrawn and on-chain uncollected fees are unaffected), leaves
  `collected_fees` empty and sets `collected_fees_unavailable`. `read_history` drops every collect
  event of an affected position and flags the history. Consumers must honour the flag rather than
  read empty collected fees as zero. The mirror check compares values numerically on both paths.

### Fixed

- **Dust no longer becomes a position.** Lido (stETH/wstETH), ether.fi (eETH/weETH), Ethena (sUSDe)
  and Pendle (PT/YT and the vePENDLE lock) skip balances that `is_dust` classifies as residue, such
  as the 1 wei of stETH left after a full exit. Dust tokens stay in `receipt_tokens`, so consumers
  that hide wrapper tokens from wallet holdings keep hiding them.

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

[Unreleased]: https://github.com/ovictormagalhaes/gluonscan/compare/v0.3.0-beta.4...HEAD
[0.3.0-beta.4]: https://github.com/ovictormagalhaes/gluonscan/compare/v0.3.0-beta.3...v0.3.0-beta.4
[0.3.0-beta.3]: https://github.com/ovictormagalhaes/gluonscan/compare/v0.3.0-beta.2...v0.3.0-beta.3
[0.3.0-beta.2]: https://github.com/ovictormagalhaes/gluonscan/compare/v0.3.0-beta.1...v0.3.0-beta.2
[0.3.0-beta.1]: https://github.com/ovictormagalhaes/gluonscan/compare/v0.2.5...v0.3.0-beta.1
[0.2.5]: https://github.com/ovictormagalhaes/gluonscan/compare/v0.2.4...v0.2.5
[0.2.4]: https://github.com/ovictormagalhaes/gluonscan/compare/v0.2.3...v0.2.4
[0.2.3]: https://github.com/ovictormagalhaes/gluonscan/compare/v0.2.2...v0.2.3
[0.2.2]: https://github.com/ovictormagalhaes/gluonscan/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/ovictormagalhaes/gluonscan/compare/v0.2.0...v0.2.1
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
