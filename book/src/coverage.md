# Supported coverage

## Protocols

| Protocol | Backend(s) | Capabilities |
|---|---|---|
| Aave V3 | API | Positions, Health factor |
| Uniswap V3 | Subgraph + on-chain | Positions, Fees |
| Pendle | API + on-chain | Positions |
| Kamino | API | Positions, Health factor |
| Raydium (CLMM) | On-chain | Positions, Fees |

## Chains

EVM: Ethereum, Base, Arbitrum, Optimism, Polygon, BNB, Monad, Hyperliquid.
Non-EVM: Solana, Bitcoin.

A protocol binds only to the chains it supports; enabling it elsewhere is a configuration error.

## Position kinds

`WalletBalance` (idle tokens not in any protocol), `LendingPosition` (supplies/borrows/health
factor), `LiquidityPosition` (principal, uncollected/deposited/withdrawn/collected fees, tick range,
in-range), `LockPosition`, `YieldPosition` (PT/YT with maturity), `NftPosition`.
