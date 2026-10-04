# gluonscan-morpho

Morpho Blue lending adapter for [gluonscan](https://crates.io/crates/gluonscan).

Reads a wallet's Morpho Blue positions from the official GraphQL API
(`https://blue-api.morpho.org/graphql`, no key) and normalizes each **isolated** market into a
`LendingPosition` with its own collateral, debt and health factor.

- **Source:** HTTP GraphQL API.
- **Chains:** Ethereum, Base.
- **Capabilities:** Positions, Health factor, Risk config.

Mapping notes:

- One `LendingPosition` per non-empty market (Morpho Blue markets are isolated). Historical/dust
  markets returned by the API (all amounts zero) are dropped.
- A market's single LLTV is both `max_ltv` and `liquidation_threshold`. Debt has no borrow factor
  (counts at 100%, encoded as `1`).
- Base-unit amounts arrive as JSON numbers; `serde_json`'s `arbitrary_precision` keeps large
  18-decimal amounts lossless.
- A position with debt but no health factor fails closed (never hide liquidation risk).
- Claimable incentives moved to Merkl (a separate per-wallet system) and are a planned follow-up;
  `rewards` is left empty rather than guessed.

Licensed under MIT OR Apache-2.0.
