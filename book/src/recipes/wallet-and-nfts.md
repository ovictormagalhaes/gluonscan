# Wallet tokens & NFTs

Idle balances and collectibles are readers like any protocol. Two capabilities, both routed as
`Protocol::` values:

- **`Protocol::Wallet`** — tokens sitting idle in the wallet, not deployed in any protocol.
- **`Protocol::Nfts`** — collectible NFTs the wallet holds.

## Idle token balances

| Backend | Chain(s) | Needs |
|---|---|---|
| `EvmWallet` | EVM (6 chains) | `HTTP` + a Moralis-style API key |
| `SolanaWallet` | Solana | `RPC` |
| `BitcoinWallet` | Bitcoin | `HTTP` |

```rust,ignore
use gluonscan::{Gluonscan, EvmWallet, Protocol, Chain, Detail, Wallet, Address};

# async fn run() -> Result<(), gluonscan::Error> {
let engine = Gluonscan::builder().backend(EvmWallet::new("MORALIS_KEY")).build();
let owner = Wallet::Evm(Address::ZERO);

let reading = engine.read(Protocol::Wallet, owner, Chain::Ethereum, Detail::Full).await?;
// each position is a Position::Wallet(WalletBalance) — token + amount, usd left None
# Ok(()) }
```

Balances carry **no price** (`usd: None`). Pricing is a [separate operation](pricing.md).

## NFTs

| Backend | Chain(s) | How it discovers |
|---|---|---|
| `EvmNfts` | EVM (6 chains) | Moralis-style `/{address}/nft`; drops spam and **protocol-position** contracts |
| `SolanaNfts` | Solana | Mints held 1×/0-decimals, by owner; decodes Metaplex name + collection |

```rust,ignore
use gluonscan::{Gluonscan, EvmNfts, Protocol, Chain, Detail, Wallet, Address};

# async fn run() -> Result<(), gluonscan::Error> {
let engine = Gluonscan::builder().backend(EvmNfts::new("MORALIS_KEY")).build();
let owner = Wallet::Evm(Address::ZERO);

let nfts = engine.read(Protocol::Nfts, owner, Chain::Ethereum, Detail::Full).await?;
// each position is a Position::Nft(NftPosition) — collection, token_id, name, floor_price (None)
# Ok(()) }
```

> **No double-counting.** A Uniswap V3 LP *is* an ERC-721, but it is read as a
> `Position::Liquidity` by the Uniswap backend — so `EvmNfts` deliberately skips the Uniswap
> NonfungiblePositionManager contract. It also drops indexer-flagged spam. What you get back are
> real collectibles, not protocol receipts.
