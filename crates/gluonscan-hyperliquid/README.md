# gluonscan-hyperliquid

Hyperliquid adapter for [gluonscan](https://crates.io/crates/gluonscan).

Reads a wallet's open perpetual positions from the public `clearinghouseState` info API
(`https://api.hyperliquid.xyz/info`, no key) and normalizes each into a `PerpPosition`, reads its
spot balances from `spotClearinghouseState`, and reads its perp fills as history.

- **Source:** HTTP info API. **Chains:** Hyperliquid. **Capabilities:** Positions, History.

Mapping notes:

- `coin` → market; the sign of `szi` → side (Long / Short), its magnitude → size.
- Mark price is derived from the notional the API already returns (`positionValue / |size|`), so no
  second call is needed and it stays consistent with the reported PnL.
- `unrealizedPnl` and `cumFunding.sinceOpen` map to USD `Money`; `marginUsed` is the USDC collateral.
  Hyperliquid is USD-margined (USDC == USD, no contract to price), so these are the API's own
  authoritative USD figures, parsed straight to `Decimal` — never through `f64`.
- A null `liquidationPx` or an absent `cumFunding` stays `None`, never a fabricated value.
- Balances are `Position::Wallet`s: USDC once, plus every other spot token priced at the mark of its
  USDC pair (left unpriced when it has none). The account mode (`userAbstraction`) decides where the
  USDC lives: a unified account's spot balance already holds the perp margin and its PnL, so it is
  the USDC alone; a separate account (`disabled`, `default`, `dexAbstraction`) adds the perp equity
  (`marginSummary.accountValue`). Portfolio margin and unrecognized modes fail closed.
- At `Detail::Full`, each position carries its resting TP/SL (`frontendOpenOrders`) and `opened_at`,
  the time of the fill that opened it (`userFills`); both are best-effort and stay `None` on failure.
- History is the perp fills from `userFillsByTime` (open, close, flip, liquidation, with price,
  realized PnL and fee); `position` optionally selects one market (e.g. `"BTC"`). Spot and
  builder-dex fills are not included. Hyperliquid serves only an
  account's most recent 10,000 fills; a history that reaches that limit sets
  `earlier_events_unavailable`.
- HLP vault deposits and builder-dex (HIP-3) perps are not read. A unified account's USDC covers
  them (one balance per asset); a separate-balance account's USDC held on a builder dex is not
  counted.

Fail-closed: an empty `assetPositions` is a valid no-position result; a malformed payload (missing
`assetPositions`) or any unparseable required field fails closed, and a transport failure is
retryable.

Licensed under MIT OR Apache-2.0.
