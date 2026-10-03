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
| `UniswapV3` | Uniswap V3 | EVM | Ethereum, Base, Arbitrum | 📈 + ⛓️ | `HTTP` (+ `RPC` for `Full` fees) | Positions, Fees |
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

`LendingPosition`, `LiquidityPosition` and `StakePosition` also carry claimable `rewards`
(emission/incentive tokens, distinct from LP trading fees) when a source exposes them — surfaced by
the `Rewards` capability.
