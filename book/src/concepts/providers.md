# Providers, wallets & execution

gluonscan owns no state and reads no global config. Everything the engine needs is **injected**.

## Ports

Adapters depend on traits, not concrete clients:

- `Http` — API/subgraph adapters (`get`/`post`).
- `ChainProvider` — an injected JSON-RPC transport (EVM `eth_call`, Solana `getAccountInfo`, …).
- `PriceSource` — prices in USD (a missing price must be an error, never `0`/`1`).
- `Clock` — deterministic, testable timestamps.

The host supplies concrete implementations (the facade wires a `reqwest`-based `Http` and a system
clock); tests supply mocks that replay recorded contracts.

## Wallets are cross-ecosystem

```rust,ignore
pub enum Wallet { Evm(Address), Solana(String) }
```

An EVM adapter resolves `owner.evm()?`; a Solana adapter resolves `owner.solana()?`. Passing the
wrong kind of wallet is a permanent error, not a silent empty result.

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
