# Supported coverage

The one table to scan before you wire anything. **Source** is how a backend reads; **Transport**
is what you must inject for it to work.

**Legend** — Source: 🌐 API · 📈 Subgraph · ⛓️ On-chain RPC. Transport: `HTTP` a
[`Http`](concepts/providers.md) client (the facade's default `reqwest` one is fine) · `RPC` an
injected [`ChainProvider`](concepts/providers.md) · `key` a provider API key.

## Protocols

| Backend | Protocol | Ecosystem | Chains | Source | Transport | Capabilities |
|---|---|---|---|---|---|---|
| `AaveApi` | Aave V3 | EVM | Ethereum, Base, Arbitrum, Optimism, Polygon, BNB | 🌐 | `HTTP` | Positions, Health factor |
| `Lido` | Lido | EVM | Ethereum | ⛓️ | `RPC` | Positions |
| `EtherFi` | ether.fi | EVM | Ethereum | ⛓️ | `RPC` | Positions |
| `Ethena` | Ethena | EVM | Ethereum | ⛓️ | `RPC` | Positions |
| `Hyperliquid` | Hyperliquid | EVM | Hyperliquid | 🌐 | `HTTP` | Positions |
| `Morpho` | Morpho Blue | EVM | Ethereum, Base | 🌐 | `HTTP` | Positions, Health factor, Risk config |
| `UniswapV3` | Uniswap V3 | EVM | Ethereum, Base, Arbitrum | 📈 + ⛓️ | `HTTP` (+ `RPC` for `Full` fees and mirrored collects) | Positions, Fees, History |
| `PendleApi` | Pendle | EVM | Ethereum, Arbitrum, Base | 🌐 + ⛓️ | `HTTP` + `RPC` | Positions |
| `KaminoApi` | Kamino | Solana | Solana | 🌐 | `HTTP` | Positions, Health factor |
| `RaydiumClmm` | Raydium (CLMM) | Solana | Solana | ⛓️ | `RPC` | Positions, Fees |

## Wallet & NFT readers

| Backend | Reads | Ecosystem | Chains | Source | Transport |
|---|---|---|---|---|---|
| `EvmWallet` | Idle ERC-20 balances | EVM | Ethereum, Base, Arbitrum, Optimism, Polygon, BNB | 🌐 | `HTTP` + `key` |
| `SolanaWallet` | Idle SPL balances | Solana | Solana | ⛓️ | `RPC` |
| `BitcoinWallet` | Native BTC balance | Bitcoin | Bitcoin | 🌐 | `HTTP` |
| `EvmNfts` | Collectible NFTs | EVM | Ethereum, Base, Arbitrum, Optimism, Polygon, BNB | 🌐 | `HTTP` + `key` |
| `SolanaNfts` | Collectible NFTs (Metaplex) | Solana | Solana | ⛓️ | `RPC` |

## Price sources

Pricing is a [separate operation](recipes/pricing.md) — never folded into a read.

| Backend | Native coin | EVM token (by contract) | Solana SPL (by mint) | Transport |
|---|---|---|---|---|
| `CoinGecko` | ✅ | ✅ | ✅ | `HTTP` |
| `CoinMarketCap` | ✅ | ✅ | ❌ (use CoinGecko) | `HTTP` + `key` |

## Chains

EVM: Ethereum, Base, Arbitrum, Optimism, Polygon, BNB, Monad, Hyperliquid.
Non-EVM: Solana, Bitcoin.

A protocol binds only to the chains it supports; enabling it elsewhere is a configuration error
surfaced at read time, never a silent empty result.

## Position kinds

`WalletBalance` (idle tokens not in any protocol), `LendingPosition` (supplies/borrows/health
factor), `LiquidityPosition` (principal, uncollected/deposited/withdrawn/collected fees, tick range,
in-range), `LockPosition`, `StakePosition`, `YieldPosition` (PT/YT with maturity), `PerpPosition`
(side, size, entry/mark/liquidation price, PnL, funding), `NftPosition`.

`YieldPosition` also names its `accounting_asset` when the source does: what one PT redeems to at
maturity (USDe for PT-sUSDe; a token without an address for a native coin such as ETH). It is
`None` when the source does not name it, never the wrapped underlying.

`LendingPosition`, `LiquidityPosition` and `StakePosition` also carry claimable `rewards`
(emission/incentive tokens, distinct from LP trading fees) when a source exposes them — surfaced by
the `Rewards` capability.

## Rates

Supplied and borrowed lending legs, `LiquidityPosition`, `StakePosition` and `YieldPosition` carry
`rates: Rates`: at most one `Rate` per `(RateBasis, RateWindow)`, each with its value as a fraction,
its `Source` and its `as_of` timestamp (the upstream timestamp when the source gives one, else the
read time). `Rates::get(basis, window)` never substitutes another window or basis; an APR is never
converted into an APY. A window whose data does not cover it is omitted, not filled.

| Backend | Basis | Windows |
|---|---|---|
| `AaveApi` | APY | `Instant` |
| `Morpho` | APY | `Instant`, `Trailing` 24h / 168h / 720h (the API's daily, weekly and monthly APYs), dated at the market state's timestamp. Collateral carries none. |
| `KaminoApi` | APY | `Instant` (`reserves/metrics`), `Trailing` 24h / 168h (mean of the reserve's hourly history; emitted with at least 90% of the hourly points and a newest point at most 3h old) |
| `PendleApi` | APY | `Implied` |
| `Lido` | APR | `Epoch` 24h (last oracle report), `Trailing` 168h (the API's 7-report SMA) |
| `EtherFi` | APR | `Epoch` of unstated length |
| `Ethena` | APY | `Epoch` 24h (newest DeFiLlama point), `Trailing` 168h (mean of the newest 7 daily points; emitted when they span at most 8 days and the newest is at most 48h old) |

Rates are informational: a failed rate request leaves that rate out and never fails the read.

## Stake underlying

`StakePosition::underlying` is index-aligned with `staked`: what each wrapped stake redeems for, read
on-chain (wstETH → stETH via `getStETHByWstETH`, weETH → eETH via `getEETHByWeETH`, sUSDe → USDe via
`convertToAssets`). The underlying growing at a constant wrapper balance is the staking yield, told
apart from a price move. A rebasing token (stETH, eETH) carries `None`, and so does a wrapper whose
rate read failed: like rates, it is best-effort and never fails the balance read.
