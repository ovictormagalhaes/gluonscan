# By source: on-chain, API, or price

Every backend declares **how** it reads — its [`Source`](../concepts/providers.md):

- 🌐 **`Api`** — a hosted HTTP API (Aave, Pendle, Kamino, the Moralis-style wallet/NFT readers,
  the price sources). Needs an [`Http`](../concepts/providers.md) client.
- 📈 **`Subgraph`** — a GraphQL indexer queried by owner (Uniswap discovery). Needs `Http`.
- ⛓️ **`OnChain`** — direct JSON-RPC / account decoding (Raydium, Solana wallet & NFTs, Uniswap
  fees, Pendle balances). Needs an injected [`ChainProvider`](../concepts/providers.md).

You inject transports once; each backend uses what it needs. The [coverage matrix](../coverage.md)
lists the source and transport per backend.

## Strictly on-chain (your own node, no third-party APIs)

Register only ⛓️ backends and inject an RPC. This is the "trust no external indexer" setup.

```rust,ignore
use gluonscan::{Gluonscan, RaydiumClmm, SolanaWallet, SolanaNfts, Protocol, Chain, Detail, Wallet};

# async fn run(rpc: std::sync::Arc<dyn gluonscan::ChainProvider>) -> Result<(), gluonscan::Error> {
let engine = Gluonscan::builder()
    .backend(RaydiumClmm::new())
    .backend(SolanaWallet::new())
    .backend(SolanaNfts::new())
    .rpc(rpc) // Arc<dyn ChainProvider> — your node
    .build();

let owner = Wallet::Solana("So1111...".to_string());
let ray = engine.read(Protocol::Raydium, owner, Chain::Solana, Detail::Full).await?;
# Ok(()) }
```

> **Honest caveat.** Not every protocol can be read purely on-chain. Uniswap V3 *discovery* is a
> subgraph query (position enumeration on-chain would mean scanning every NFT); only its **fees**
> are on-chain. Pendle discovery is an API. The matrix marks these `📈`/`🌐`.

## Hosted APIs only (no node to run)

Register only 🌐 backends. No `rpc` needed.

```rust,ignore
use gluonscan::{Gluonscan, AaveApi, KaminoApi, BitcoinWallet, Protocol, Chain, Detail, Wallet, Address};

# async fn run() -> Result<(), gluonscan::Error> {
let engine = Gluonscan::builder()
    .backend(AaveApi::new())
    .backend(KaminoApi::new())
    .backend(BitcoinWallet::new())
    .build(); // the default reqwest Http is wired for you

let evm = Wallet::Evm(Address::ZERO);
let aave = engine.read(Protocol::AaveV3, evm, Chain::Ethereum, Detail::Full).await?;
# Ok(()) }
```

> Backends that decode on-chain state still need an `rpc` even when their *discovery* is an API —
> Pendle (balances) and Uniswap `Full` fees are the current cases. Register them only if you also
> call `.rpc(...)`.

## A mix (the common case)

Inject both and register whatever you need; each backend picks its own transport.

```rust,ignore
# async fn run(rpc: std::sync::Arc<dyn gluonscan::ChainProvider>) -> Result<(), gluonscan::Error> {
use gluonscan::{Gluonscan, AaveApi, UniswapV3, RaydiumClmm};

let engine = Gluonscan::builder()
    .backend(AaveApi::new())      // 🌐 uses Http
    .backend(UniswapV3::new())    // 📈 discovery + ⛓️ fees
    .backend(RaydiumClmm::new())  // ⛓️ uses RPC
    .rpc(rpc)
    .build();
# Ok(()) }
```

For pricing as a source of its own, see [Pricing](pricing.md).
