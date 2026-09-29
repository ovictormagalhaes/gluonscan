# Quickstart

Add the facade crate (pick the protocols you need via features):

```toml
[dependencies]
gluonscan = { git = "https://github.com/ovictormagalhaes/gluonscan", tag = "v0.1.0" }
```

Build an engine, register the backends you want, and read:

```rust,ignore
use gluonscan::{Gluonscan, AaveApi, UniswapV3, Protocol, Chain, Detail, Wallet, Address};

# async fn run() -> Result<(), gluonscan::Error> {
let engine = Gluonscan::builder()
    .backend(AaveApi::new())
    .backend(UniswapV3::new())
    .build();

let owner = Wallet::Evm("0xABCD...".parse::<Address>().unwrap());

// Everything Aave exposes for this wallet on Ethereum, fully.
let aave = engine.read(Protocol::AaveV3, owner.clone(), Chain::Ethereum, Detail::Full).await?;

// Just "does this wallet hold a Uniswap position?" — the cheapest tier.
let uni = engine.read(Protocol::UniswapV3, owner, Chain::Ethereum, Detail::Presence).await?;
# Ok(()) }
```

The returned value is `Complete<Reading>` — a reading that is guaranteed complete, or an
[`Error`](concepts/integrity.md). The domain types (`Reading`, `Position`, `LendingPosition`,
`LiquidityPosition`, `YieldPosition`, `Money`, `Provenance`) are the contract: map them to whatever
your app needs.

On-chain protocols (Uniswap fees, Raydium) need an injected RPC transport:

```rust,ignore
let engine = Gluonscan::builder()
    .backend(UniswapV3::new())
    .rpc(my_chain_provider) // Arc<dyn ChainProvider>
    .build();
```
