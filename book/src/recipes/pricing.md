# Pricing (a separate operation)

Reading positions and pricing them are **two distinct calls**. A `read` never fetches prices, and
`price` never reads positions. This keeps reads cheap and deterministic, lets you price on your own
schedule (and cache), and means a price outage can never corrupt a position read.

Register a price source, then call `price` with an [`Asset`](../concepts/providers.md):

```rust,ignore
use gluonscan::{Gluonscan, CoinGecko, Asset, Chain, Address, ReqwestHttp};
use std::sync::Arc;

# async fn run() -> Result<(), gluonscan::Error> {
let engine = Gluonscan::builder()
    .price_source(Arc::new(CoinGecko::new(Arc::new(ReqwestHttp::new()))))
    .build();

// Native coin of a chain (BTC, ETH, SOL…):
let btc = engine.price(Chain::Bitcoin, Asset::Native).await?;

// An EVM token by contract:
let weth: Address = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2".parse().unwrap();
let weth_usd = engine.price(Chain::Ethereum, Asset::Token(weth)).await?;

// A Solana SPL token by mint:
let usdc = engine.price(Chain::Solana, Asset::Mint("EPjF...".to_string())).await?;
# Ok(()) }
```

## The `Asset` key is chain-agnostic

An EVM contract address can't name native BTC or a base58 SPL mint, so pricing keys on `Asset`:

| `Asset` | Prices | CoinGecko | CoinMarketCap |
|---|---|---|---|
| `Native` | the chain's native coin | ✅ | ✅ (by symbol) |
| `Token(Address)` | an EVM token by contract | ✅ | ✅ (address→id) |
| `Mint(String)` | a Solana SPL token by mint | ✅ | ❌ `Error::Permanent` |

A missing price is always [`Error::AbsentPrice`](../concepts/integrity.md) — **never** a fabricated
`0` or `1`. CoinMarketCap cannot resolve SPL mints; it says so with a permanent error rather than
guessing.

## Rate limiting

Rate limits are **per provider**. Wrap the transport in `RateLimitedHttp` — CoinGecko's free/demo
tier is roughly 30 requests/minute:

```rust,ignore
use gluonscan::{CoinGecko, RateLimitedHttp, ReqwestHttp};
use std::sync::Arc;

let http = Arc::new(RateLimitedHttp::per_minute(Arc::new(ReqwestHttp::new()), 30));
let coingecko = CoinGecko::new(http);
```

Give each source its own `RateLimitedHttp` for its own budget, or share one instance to share a
budget across chains on the same key.

## The two-call pattern

Read positions, then price the tokens you saw — on your own cadence, caching between calls:

```rust,ignore
# async fn run(engine: &gluonscan::Gluonscan, reading: &gluonscan::Reading) -> Result<(), gluonscan::Error> {
use gluonscan::{Asset, Chain, Position};

for position in &reading.positions {
    if let Position::Wallet(bal) = position {
        if let Some(addr) = bal.amount.token.address {
            let usd = engine.price(Chain::Ethereum, Asset::Token(addr)).await?;
            // combine usd with bal.amount.amount in your own layer
        }
    }
}
# Ok(()) }
```
