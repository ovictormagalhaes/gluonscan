//! # gluonscan
//!
//! The facade: re-exports the [`gluonscan_core`] contract, bundles the protocol adapters, and
//! wires real transports (a `reqwest` HTTP client, a system clock). Configure backends, then call
//! the read methods — conversions, math and normalization stay hidden inside the adapters.
//!
//! ```no_run
//! # async fn demo() -> Result<(), gluonscan::Error> {
//! use gluonscan::{Gluonscan, AaveApi, Chain, Detail, Protocol, Address, Wallet};
//!
//! let engine = Gluonscan::builder().backend(AaveApi::new()).build();
//! let reading = engine
//!     .read(Protocol::AaveV3, Wallet::Evm(Address::ZERO), Chain::Ethereum, Detail::Full)
//!     .await?;
//! println!("{:?}", reading.get());
//! # Ok(()) }
//! ```

use std::sync::Arc;

use async_trait::async_trait;
pub use gluonscan_aave::AaveApi;
pub use gluonscan_core::*;
pub use gluonscan_kamino::KaminoApi;
pub use gluonscan_pendle::PendleApi;
pub use gluonscan_raydium::RaydiumClmm;
pub use gluonscan_sources::{CoinGecko, CoinMarketCap};
pub use gluonscan_uniswap::UniswapV3;

/// A `reqwest`-backed [`Http`] client. Configuration (timeouts, keys, retries) lives here, not in
/// the adapters.
pub struct ReqwestHttp {
    client: reqwest::Client,
}

impl ReqwestHttp {
    /// A client with default settings.
    pub fn new() -> Self {
        ReqwestHttp {
            client: reqwest::Client::new(),
        }
    }
}

impl Default for ReqwestHttp {
    fn default() -> Self {
        ReqwestHttp::new()
    }
}

#[async_trait]
impl Http for ReqwestHttp {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> Result<String, Error> {
        let mut req = self
            .client
            .post(url)
            .header("content-type", "application/json")
            .body(body);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        read_body(req).await
    }

    async fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<String, Error> {
        let mut req = self.client.get(url);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        read_body(req).await
    }
}

async fn read_body(req: reqwest::RequestBuilder) -> Result<String, Error> {
    let resp = req.send().await.map_err(|e| Error::Transient {
        message: e.to_string(),
        retry_after: None,
    })?;
    if resp.status().as_u16() == 429 {
        return Err(Error::Transient {
            message: "HTTP 429".into(),
            retry_after: None,
        });
    }
    resp.text().await.map_err(|e| Error::Transient {
        message: e.to_string(),
        retry_after: None,
    })
}

/// A clock backed by the system time.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        Timestamp(secs)
    }
}

/// Builds a [`Gluonscan`] engine: register protocol backends, optionally override the transports.
pub struct Builder {
    adapters: Vec<Arc<dyn ProtocolAdapter>>,
    http: Option<Arc<dyn Http>>,
    rpc: Option<Arc<dyn ChainProvider>>,
    price: Option<Arc<dyn PriceSource>>,
    clock: Option<Arc<dyn Clock>>,
}

impl Builder {
    /// Register a protocol backend (e.g. [`AaveApi`]). Multiple backends per protocol are allowed.
    pub fn backend<A: ProtocolAdapter + 'static>(mut self, adapter: A) -> Self {
        self.adapters.push(Arc::new(adapter));
        self
    }

    /// Override the HTTP client (defaults to [`ReqwestHttp`]).
    pub fn http(mut self, http: Arc<dyn Http>) -> Self {
        self.http = Some(http);
        self
    }

    /// Override the clock (defaults to [`SystemClock`]).
    pub fn clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = Some(clock);
        self
    }

    /// Attach an on-chain transport (required by on-chain protocols such as Uniswap fees).
    pub fn rpc(mut self, rpc: Arc<dyn ChainProvider>) -> Self {
        self.rpc = Some(rpc);
        self
    }

    /// Register a price source, enabling the separate [`Gluonscan::price`] operation. Pricing is
    /// never folded into `read` — reading positions and pricing tokens are two operations.
    pub fn price_source(mut self, price: Arc<dyn PriceSource>) -> Self {
        self.price = Some(price);
        self
    }

    /// Finish building.
    pub fn build(self) -> Gluonscan {
        let http = self.http.unwrap_or_else(|| Arc::new(ReqwestHttp::new()));
        let clock = self.clock.unwrap_or_else(|| Arc::new(SystemClock));
        let mut cx = Ctx::new(http, clock);
        if let Some(rpc) = self.rpc {
            cx = cx.with_rpc(rpc);
        }
        Gluonscan {
            adapters: self.adapters,
            price: self.price,
            cx,
        }
    }
}

/// The configured engine.
pub struct Gluonscan {
    adapters: Vec<Arc<dyn ProtocolAdapter>>,
    price: Option<Arc<dyn PriceSource>>,
    cx: Ctx,
}

impl Gluonscan {
    /// Start configuring an engine.
    pub fn builder() -> Builder {
        Builder {
            adapters: Vec::new(),
            http: None,
            rpc: None,
            price: None,
            clock: None,
        }
    }

    /// Read one protocol for `owner` on `chain`. Fails with [`Error::Permanent`] if no registered
    /// backend for that protocol supports the chain (a protocol binds only to chains it supports).
    pub async fn read(
        &self,
        protocol: Protocol,
        owner: Wallet,
        chain: Chain,
        detail: Detail,
    ) -> Result<Complete<Reading>, Error> {
        let adapter = self
            .adapters
            .iter()
            .find(|a| a.protocol() == protocol && a.supported_chains().contains(&chain))
            .ok_or_else(|| Error::Permanent {
                message: format!("no registered {protocol:?} backend supports {chain:?}"),
            })?;
        adapter.read(&owner, chain, detail, &self.cx).await
    }

    /// Price a token in USD via the configured price source — a **separate** operation from
    /// [`read`](Gluonscan::read). Reading positions and pricing tokens are two distinct calls.
    /// Errors with [`Error::Permanent`] if no price source was registered.
    pub async fn price(
        &self,
        chain: Chain,
        token: Address,
    ) -> Result<rust_decimal::Decimal, Error> {
        self.price
            .as_ref()
            .ok_or_else(|| Error::Permanent {
                message: "no price source configured".into(),
            })?
            .price_usd(chain, token)
            .await
    }
}
