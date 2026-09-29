# By protocol: one or all

## Just one protocol

Register a single backend and read it. Use [detail levels](../concepts/detail-levels.md) to pay only
for what you need — `Presence` to answer "does this wallet have an Aave position?", `Full` for every
number.

```rust,ignore
use gluonscan::{Gluonscan, AaveApi, Protocol, Chain, Detail, Wallet, Address};

# async fn run() -> Result<(), gluonscan::Error> {
let engine = Gluonscan::builder().backend(AaveApi::new()).build();
let owner = Wallet::Evm(Address::ZERO);

let reading = engine.read(Protocol::AaveV3, owner, Chain::Ethereum, Detail::Full).await?;
for position in &reading.get().positions {
    // match on Position::Lending { .. } etc.
    println!("{position:?}");
}
# Ok(()) }
```

## All the protocols a wallet touches

The engine is **stateless and does not fan out for you** — you own the orchestration, because only
you know which `(protocol, chain)` pairs are worth a request and how much concurrency your rate
limits allow. Register every backend, then drive the loop:

```rust,ignore
use gluonscan::{Gluonscan, AaveApi, UniswapV3, PendleApi, Protocol, Chain, Detail, Wallet, Address};

# async fn run(rpc: std::sync::Arc<dyn gluonscan::ChainProvider>) -> Result<(), gluonscan::Error> {
let engine = Gluonscan::builder()
    .backend(AaveApi::new())
    .backend(UniswapV3::new())
    .backend(PendleApi::new())
    .rpc(rpc)
    .build();

let owner = Wallet::Evm(Address::ZERO);
let targets = [
    (Protocol::AaveV3, Chain::Ethereum),
    (Protocol::UniswapV3, Chain::Base),
    (Protocol::Pendle, Chain::Arbitrum),
];

for (protocol, chain) in targets {
    match engine.read(protocol, owner.clone(), chain, Detail::Full).await {
        Ok(reading) => { /* persist reading.get() */ }
        Err(e) if e.is_retryable() => { /* back off and retry this pair */ }
        Err(e) => { /* permanent/integrity: skip this pair, keep the others */ eprintln!("{e}"); }
    }
}
# Ok(()) }
```

Each read is **independent**: one pair failing (a provider hiccup, an unsupported chain) never
corrupts or blocks the others. That is the [integrity contract](../concepts/integrity.md) at work —
per read, it is complete or an error, and you decide the policy.

> **Binding.** If you ask for `(Protocol::AaveV3, Chain::Solana)` and no registered Aave backend
> supports Solana, you get an `Error::Permanent` — not an empty reading. Register backends only for
> the chains you mean to read; see the [coverage matrix](../coverage.md).
