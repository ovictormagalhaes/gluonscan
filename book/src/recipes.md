# Recipes by use case

gluonscan is one engine, but no two callers use it the same way. One prices a single Aave position
from a hosted API; another runs its own Solana node and wants every on-chain position, no
third-party services; a third only ever touches Bitcoin. Each of these is a few lines — the trick
is knowing *which* few.

Pick the axis that matches how you think about your problem:

- **[By source](recipes/by-source.md)** — do you read from hosted **APIs**, strictly **on-chain**
  over your own RPC, or a mix? This decides what transport you inject.
- **[By protocol](recipes/by-protocol.md)** — one protocol, or all of them? This decides which
  backends you register and how you drive the reads.
- **[By ecosystem](recipes/by-ecosystem.md)** — EVM only, Solana only, Bitcoin only, or several?
  This decides your `Wallet` variants and chains.
- **[Wallet tokens & NFTs](recipes/wallet-and-nfts.md)** — idle balances and collectibles, which
  are readers like any protocol.
- **[Pricing](recipes/pricing.md)** — putting USD on the numbers, always a **separate** operation.

Every recipe is self-contained and copy-pasteable. They all share three rules:

1. You **register backends** and **inject transports** on the [`builder`](concepts/providers.md);
   the engine owns no state and no global config.
2. A read is **complete or an error** — never partial. See the
   [integrity contract](concepts/integrity.md).
3. A backend binds **only to the chains it supports**; asking for an unsupported chain is an error,
   not a silent empty result.
