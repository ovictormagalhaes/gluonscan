//! Chains and ecosystems.

/// A broad on-chain ecosystem. Determines which kind of access an adapter needs.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Ecosystem {
    /// EVM-compatible chains.
    Evm,
    /// Solana.
    Solana,
    /// Bitcoin.
    Bitcoin,
}

/// A supported chain. `#[non_exhaustive]` so adding a chain is never a breaking change.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Chain {
    /// Ethereum mainnet.
    Ethereum,
    /// Base.
    Base,
    /// Arbitrum One.
    Arbitrum,
    /// OP Mainnet.
    Optimism,
    /// Polygon PoS.
    Polygon,
    /// BNB Smart Chain.
    Bnb,
    /// Monad.
    Monad,
    /// Hyperliquid EVM.
    Hyperliquid,
    /// Solana.
    Solana,
    /// Bitcoin.
    Bitcoin,
}

impl Chain {
    /// The ecosystem this chain belongs to.
    pub fn ecosystem(self) -> Ecosystem {
        match self {
            Chain::Solana => Ecosystem::Solana,
            Chain::Bitcoin => Ecosystem::Bitcoin,
            _ => Ecosystem::Evm,
        }
    }

    /// The EVM chain id, or `None` for non-EVM chains.
    pub fn evm_chain_id(self) -> Option<u64> {
        Some(match self {
            Chain::Ethereum => 1,
            Chain::Optimism => 10,
            Chain::Bnb => 56,
            Chain::Polygon => 137,
            Chain::Base => 8453,
            Chain::Arbitrum => 42161,
            Chain::Monad => 143,
            Chain::Hyperliquid => 999,
            Chain::Solana | Chain::Bitcoin => return None,
        })
    }
}
