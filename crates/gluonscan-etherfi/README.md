# gluonscan-etherfi

ether.fi liquid-restaking adapter for [gluonscan](https://crates.io/crates/gluonscan).

Reads a wallet's **weETH** and **eETH** balances on Ethereum via on-chain `balanceOf` and returns
them as a `StakePosition`. The tokens are reported as `receipt_tokens` so a consumer that also lists
idle wallet balances drops them and does not double-count the stake.

- **Source:** on-chain RPC (`ChainProvider`).
- **Chains:** Ethereum. (Bridged weETH on L2s is a planned follow-up: the L2 tokens are plain OFT
  ERC-20s with no local rate, so valuation needs the mainnet rate.)
- **Capabilities:** Positions.

Restaking yield accrues inside the balance (eETH rebases; weETH's exchange rate rises), and the
ether.fi points / ETHFI / EIGEN rewards are off-chain season-gated Merkle claims rather than a
continuously-accruing on-chain balance, so `rewards` is always empty. Valuation is left to the
pricing layer (weETH and eETH are both priced by contract address), so the read never converts or
fabricates an amount.

Licensed under MIT OR Apache-2.0.
