//! Ports: the injected traits an adapter depends on, and the [`ProtocolAdapter`] port itself.
//!
//! Nothing here performs I/O directly. The engine (or the facade) supplies concrete
//! implementations; tests supply mocks. This is what keeps the engine stateless and reproducible.

use crate::{
    Asset, Capability, Chain, Complete, Detail, Error, Protocol, Reading, Source, Timestamp, Wallet,
};
use async_trait::async_trait;
use std::sync::Arc;

/// An injected HTTP client. API-based adapters use this instead of owning a client, so keys,
/// timeouts, retries and rate limits live with the host, and tests can replay recorded fixtures.
#[async_trait]
pub trait Http: Send + Sync + 'static {
    /// POST a request body to `url` with extra request headers, returning the raw response body.
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> Result<String, Error>;

    /// GET `url` with extra request headers, returning the raw response body.
    async fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<String, Error>;
}

/// An injected JSON-RPC / on-chain transport. Used by on-chain adapters (not by API adapters).
#[async_trait]
pub trait ChainProvider: Send + Sync + 'static {
    /// Perform a raw JSON-RPC call and return the raw response body.
    async fn call(&self, chain: Chain, method: &str, params: String) -> Result<String, Error>;
}

/// An injected price source. Missing prices must surface as [`Error::AbsentPrice`], never `0`/`1`.
#[async_trait]
pub trait PriceSource: Send + Sync + 'static {
    /// Price an [`Asset`] on a chain in USD. The asset key is chain-agnostic, so native coins
    /// (BTC, ETH, SOL) and Solana SPL mints are priceable, not just EVM token contracts.
    async fn price_usd(&self, chain: Chain, asset: Asset) -> Result<rust_decimal::Decimal, Error>;
}

/// An injected clock, so reads carry deterministic, testable timestamps.
pub trait Clock: Send + Sync + 'static {
    /// Current time.
    fn now(&self) -> Timestamp;
}

/// The dependencies handed to an adapter for a fetch. Extended over time (price source, rate
/// limiters) without changing adapter signatures.
#[derive(Clone)]
pub struct Ctx {
    /// Injected HTTP client for API/subgraph adapters.
    pub http: Arc<dyn Http>,
    /// Injected on-chain transport, when configured. `None` for API-only setups.
    pub rpc: Option<Arc<dyn ChainProvider>>,
    /// Injected clock.
    pub clock: Arc<dyn Clock>,
}

impl Ctx {
    /// Build a context from an HTTP client and a clock (no on-chain transport).
    pub fn new(http: Arc<dyn Http>, clock: Arc<dyn Clock>) -> Self {
        Ctx {
            http,
            rpc: None,
            clock,
        }
    }

    /// Attach an on-chain transport.
    pub fn with_rpc(mut self, rpc: Arc<dyn ChainProvider>) -> Self {
        self.rpc = Some(rpc);
        self
    }

    /// The on-chain transport, or a permanent error if none was configured.
    pub fn rpc(&self) -> Result<&Arc<dyn ChainProvider>, Error> {
        self.rpc.as_ref().ok_or_else(|| Error::Permanent {
            message: "this read needs an on-chain provider (RPC), but none was configured".into(),
        })
    }
}

/// A protocol adapter **backend**. A protocol may have several (e.g. an API backend and an
/// on-chain backend); the engine routes capabilities to the backend that serves them.
///
/// An adapter only fetches and normalizes — it hides the path, the conversions, and the math. It
/// returns [`Complete<Reading>`] on success, or fails closed.
#[async_trait]
pub trait ProtocolAdapter: Send + Sync + 'static {
    /// The protocol this backend serves.
    fn protocol(&self) -> Protocol;

    /// Which backend this is (for capability routing and provenance).
    fn source(&self) -> Source;

    /// The capabilities this backend can fetch. The union across a protocol's backends is its
    /// coverage; a capability none supports yields [`Error::Unsupported`].
    fn capabilities(&self) -> &'static [Capability];

    /// The chains this backend supports. The engine binds the protocol only to these — enabling it
    /// on an unsupported chain is a configuration error, not a silent no-op.
    fn supported_chains(&self) -> &'static [Chain];

    /// Read `owner`'s position for this protocol on `chain` at the requested `detail`.
    async fn read(
        &self,
        owner: &Wallet,
        chain: Chain,
        detail: Detail,
        cx: &Ctx,
    ) -> Result<Complete<Reading>, Error>;
}
