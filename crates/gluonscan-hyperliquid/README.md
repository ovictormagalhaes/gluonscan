# gluonscan-hyperliquid

Hyperliquid perpetuals adapter for [gluonscan](https://crates.io/crates/gluonscan).

Reads a wallet's open perpetual positions from the public `clearinghouseState` info API
(`https://api.hyperliquid.xyz/info`, no key) and normalizes each into a `PerpPosition`.

- **Source:** HTTP info API. **Chains:** Hyperliquid. **Capabilities:** Positions.

Mapping notes:

- `coin` → market; the sign of `szi` → side (Long / Short), its magnitude → size.
- Mark price is derived from the notional the API already returns (`positionValue / |size|`), so no
  second call is needed and it stays consistent with the reported PnL.
- `unrealizedPnl` and `cumFunding.sinceOpen` map to USD `Money`; `marginUsed` is the USDC collateral.
  Hyperliquid is USD-margined (USDC == USD, no contract to price), so these are the API's own
  authoritative USD figures, parsed straight to `Decimal` — never through `f64`.
- A null `liquidationPx` or an absent `cumFunding` stays `None`, never a fabricated value.
- Account-level equity, spot balances and HLP vault deposits are separate surfaces, not part of a
  `PerpPosition`.

Fail-closed: an empty `assetPositions` is a valid no-position result; a malformed payload (missing
`assetPositions`) or any unparseable required field fails closed, and a transport failure is
retryable.

Licensed under MIT OR Apache-2.0.
