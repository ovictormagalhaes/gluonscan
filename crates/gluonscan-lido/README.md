# gluonscan-lido

Lido liquid-staking adapter for [gluonscan](https://crates.io/crates/gluonscan).

Reads a wallet's **stETH** and **wstETH** balances on Ethereum via on-chain `balanceOf` and returns
them as a `StakePosition`. The tokens are reported as `receipt_tokens` so a consumer that also lists
idle wallet balances drops them and does not double-count the stake.

- **Source:** on-chain RPC (`ChainProvider`).
- **Chains:** Ethereum. (Bridged wstETH on L2s is a planned follow-up: its conversion rate lives on
  mainnet, so it needs a cross-chain read.)
- **Capabilities:** Positions.

Rewards accrue through the daily rebase (stETH `balanceOf` grows; wstETH's stETH value grows) and
are never separately claimable, so `rewards` is always empty. Valuation is left to the pricing layer
(stETH and wstETH are both priced by contract address), so the read never converts or fabricates an
amount.

Licensed under MIT OR Apache-2.0.
