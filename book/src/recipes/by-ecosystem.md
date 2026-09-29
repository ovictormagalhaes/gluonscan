# By ecosystem: EVM, Solana, or Bitcoin

The `Wallet` you pass must match the ecosystem, and each backend supports only its chains. Pick the
ecosystem you care about and register just those backends.

## EVM only

```rust,ignore
use gluonscan::{Gluonscan, AaveApi, UniswapV3, PendleApi, EvmWallet, EvmNfts, Protocol, Chain, Detail, Wallet, Address};

# async fn run(rpc: std::sync::Arc<dyn gluonscan::ChainProvider>) -> Result<(), gluonscan::Error> {
let engine = Gluonscan::builder()
    .backend(AaveApi::new())
    .backend(UniswapV3::new())
    .backend(PendleApi::new())
    .backend(EvmWallet::new("MORALIS_KEY"))
    .backend(EvmNfts::new("MORALIS_KEY"))
    .rpc(rpc) // Uniswap Full fees + Pendle balances read on-chain
    .build();

let owner = Wallet::Evm("0xABCD...".parse::<Address>().unwrap());
let aave = engine.read(Protocol::AaveV3, owner, Chain::Ethereum, Detail::Full).await?;
# Ok(()) }
```

Chains: Ethereum, Base, Arbitrum, Optimism, Polygon, BNB (per backend — see the
[matrix](../coverage.md)).

## Solana only

Everything Solana except Kamino reads **on-chain**, so inject an `rpc`.

```rust,ignore
use gluonscan::{Gluonscan, KaminoApi, RaydiumClmm, SolanaWallet, SolanaNfts, Protocol, Chain, Detail, Wallet};

# async fn run(rpc: std::sync::Arc<dyn gluonscan::ChainProvider>) -> Result<(), gluonscan::Error> {
let engine = Gluonscan::builder()
    .backend(KaminoApi::new())     // 🌐 lending
    .backend(RaydiumClmm::new())   // ⛓️ liquidity
    .backend(SolanaWallet::new())  // ⛓️ SPL balances
    .backend(SolanaNfts::new())    // ⛓️ NFTs (Metaplex)
    .rpc(rpc)
    .build();

let owner = Wallet::Solana("So1111...".to_string());
let ray = engine.read(Protocol::Raydium, owner, Chain::Solana, Detail::Full).await?;
# Ok(()) }
```

## Bitcoin only

Native BTC balance from an explorer API — no node, no key.

```rust,ignore
use gluonscan::{Gluonscan, BitcoinWallet, Protocol, Chain, Detail, Wallet};

# async fn run() -> Result<(), gluonscan::Error> {
let engine = Gluonscan::builder().backend(BitcoinWallet::new()).build();

let owner = Wallet::Bitcoin("bc1q...".to_string());
let btc = engine.read(Protocol::Wallet, owner, Chain::Bitcoin, Detail::Full).await?;
# Ok(()) }
```

## Several ecosystems at once

Register backends from each and pass the matching `Wallet` variant per read. Passing the wrong
wallet kind (a `Wallet::Solana` to an EVM backend) is an `Error::Permanent`, never a silent empty
result — so a misrouted call surfaces loudly instead of returning "nothing found".
