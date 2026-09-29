# Providers, wallets & execution

gluonscan owns no state and reads no global config. Everything the engine needs is **injected**.

## Ports

Adapters depend on traits, not concrete clients:

- `Http` — API/subgraph adapters (`get`/`post`).
- `ChainProvider` — an injected JSON-RPC transport (EVM `eth_call`, Solana `getAccountInfo`, …).
- `PriceSource` — prices an `Asset` in USD (a missing price must be an error, never `0`/`1`). The
  `Asset` key is chain-agnostic (`Native` / `Token(Address)` / `Mint(String)`), so native coins
  (BTC, ETH, SOL) and Solana SPL mints price too — not just EVM token contracts. Pricing is a
  **separate** operation from reading positions.
- `Clock` — deterministic, testable timestamps.

The host supplies concrete implementations (the facade wires a `reqwest`-based `Http` and a system
clock); tests supply mocks that replay recorded contracts.

## The HTTP transport

`ReqwestHttp` is the default `Http` client. It carries a **default `User-Agent`** on every request
(GET and POST, across every provider):

```text
gluonscan/<version> (+https://github.com/ovictormagalhaes/gluonscan)
```

This is not cosmetic. Some provider edges — CoinGecko's CloudFront among them — reject requests
that arrive **without** a `User-Agent` with a `403`, and `reqwest` sends none by default. A client
with no `User-Agent` would silently 403 the whole integration, so gluonscan always sends one.

Override it with your own identifier when you want traffic attributed to your app:

```rust,ignore
use gluonscan::ReqwestHttp;

let http = ReqwestHttp::with_user_agent("myapp/2.1 (+https://myapp.example)");
```

Rate limiting is layered on top, per provider (never a hidden global cap). Wrap the transport in
`RateLimitedHttp` — see [Pricing](../recipes/pricing.md#rate-limiting) for CoinGecko's ~30 req/min
free tier.

## Wallets are cross-ecosystem

```rust,ignore
pub enum Wallet { Evm(Address), Solana(String), Bitcoin(String) }
```

An EVM adapter resolves `owner.evm()?`; a Solana adapter resolves `owner.solana()?`; a Bitcoin
reader resolves `owner.bitcoin()?`. Passing the wrong kind of wallet is a permanent error, not a
silent empty result.

## A protocol binds only to the chains it supports

Each adapter declares `supported_chains()`. Enabling a protocol on a chain it doesn't support is a
configuration error surfaced at read time — never a silent no-op or a zero reading.

## Backends and capability routing

A protocol is an identity; a **backend** (an `Api`, `Subgraph`, or `OnChain` implementation) is a
configured choice. Register several backends for one protocol and route each capability to the one
that serves it best; the union of their capabilities is the protocol's coverage.

## Execution

Fetching and scheduling are separate. Adapters only fetch and normalize; a host-set execution
policy decides sequential vs. bounded-parallel. Rate limiting is a property of the provider and is
enforced per provider, not by a hidden global cap.
