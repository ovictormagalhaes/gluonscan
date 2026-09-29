# Detail levels

A caller requests a **minimum** detail level per read. That request is also the **cost ceiling**
the caller accepts: the adapter runs the cheapest fetch plan that satisfies it.

```rust,ignore
pub enum Detail { Presence, Summary, Full }
```

- `Presence` — does the wallet hold anything in this protocol?
- `Summary` — what is there, at coarse resolution.
- `Full` — everything: exact amounts, fees, ranges, health factor.

The level chooses *which resources* to fetch, not *which fields* to keep (see
[return completeness](integrity.md#return-completeness)). Receiving richer-than-requested data when
it is free is fine; doing extra *work* you didn't ask for is not.

## Cost is real, and per protocol

Each adapter only makes the calls a level needs. For example:

- **Aave** — one API call returns supplies + borrows already; the health factor and per-asset risk
  are extra calls, so `Full` costs more than `Presence`.
- **Uniswap V3** — one subgraph query yields the position, range, and lifetime totals; only the
  on-chain uncollected-fees `collect()` is gated to `Full`.
- **Pendle** — the market catalog is a prerequisite, then `balanceOf` per PT/YT.

Where fetching everything costs the same as the minimum, an adapter simply offers a single level —
there is no cheaper tier to expose.
