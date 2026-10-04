# gluonscan-ethena

Ethena staked-USDe adapter for [gluonscan](https://crates.io/crates/gluonscan).

Reads a wallet's **sUSDe** balance on Ethereum via on-chain `balanceOf` and returns it as a
`StakePosition`. sUSDe (the ERC-4626 staked-USDe vault share) is the Ethena staking position; its
yield accrues as the share's redemption value rises. sUSDe is reported as a `receipt_token` so a
consumer that also lists idle wallet balances drops it and does not double-count the stake.

- **Source:** on-chain RPC (`ChainProvider`).
- **Chains:** Ethereum. (Bridged sUSDe on L2s is a plain OFT ERC-20 whose vault interface reverts, so
  it is a separate, price-only follow-up.)
- **Capabilities:** Positions.

USDe held directly is the plain (unstaked) stablecoin, not a protocol position, so it is left to the
idle-wallet reader. Yield accrues inside the sUSDe share price, not as a separate claimable, so
`rewards` is always empty. Valuation is left to the pricing layer (sUSDe is priced by contract
address), so the read never converts or fabricates an amount. The cooldown/unstake state (sUSDe
burned into a silo with an unlock time) is a planned `LockPosition` follow-up.

Licensed under MIT OR Apache-2.0.
